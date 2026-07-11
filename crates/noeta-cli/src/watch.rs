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
use std::path::Path;
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
    let hot = args.first().is_some_and(|a| a == "serve");
    loop {
        let mut cmd = Command::new(&exe);
        cmd.args(args);
        if hot {
            cmd.env("NOETA_HOT", "1");
        }
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
            wait_for_change(&rx);
        } else {
            let exited = run_until_change(&rx, &mut child);
            if let Some(status) = exited {
                // The program finished on its own (a `run` reaching its end, a crash): report and
                // wait for the next change rather than looping hot.
                eprintln!("[watch] finished ({status}) — waiting for changes");
                wait_for_change(&rx);
            } else {
                // A relevant change arrived while the program was running: stop it and restart.
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        eprintln!("[watch] change detected — restarting");
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
) -> Option<std::process::ExitStatus> {
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        match rx.recv_timeout(POLL) {
            Ok(event) if relevant(&event) => {
                drain(rx);
                return None;
            }
            // Irrelevant event, timeout tick, or a watcher error: keep polling. A disconnected
            // watcher can produce no further events; treat it like a quiet tick (the child keeps
            // running — watch just degrades to plain execution).
            _ => {}
        }
    }
}

/// Block until a relevant change arrives (used after the child finished on its own).
fn wait_for_change(rx: &mpsc::Receiver<notify::Result<notify::Event>>) {
    loop {
        match rx.recv() {
            Ok(event) if relevant(&event) => {
                drain(rx);
                return;
            }
            Ok(_) => {}
            Err(_) => {
                // Watcher gone — nothing further can arrive; park indefinitely rather than spin.
                std::thread::park();
            }
        }
    }
}

/// Debounce: keep draining events for [`DEBOUNCE`] after the first relevant one.
fn drain(rx: &mpsc::Receiver<notify::Result<notify::Event>>) {
    while rx.recv_timeout(DEBOUNCE).is_ok() {}
}

fn relevant(event: &notify::Result<notify::Event>) -> bool {
    let Ok(event) = event else { return false };
    // Mutations only. Access events MUST be ignored — the restarted program *reads* its own
    // sources to compile them, and reacting to those reads is a restart storm.
    let mutation = matches!(
        event.kind,
        notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)
    );
    mutation && event.paths.iter().any(|p| relevant_path(p))
}

/// A project source path: `*.noe` or a manifest/lockfile, not inside a hidden directory.
fn relevant_path(path: &Path) -> bool {
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
    is_noe || is_manifest
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
    wake: std::sync::Arc<noeta_runtime::Notify>,
) {
    std::thread::spawn(move || hot_watcher(entry, mailbox, wake));
}

fn hot_watcher(
    entry: std::path::PathBuf,
    mailbox: noeta_vm::HotSwapMailbox,
    wake: std::sync::Arc<noeta_runtime::Notify>,
) {
    let entry_canon = entry.canonicalize().unwrap_or_else(|_| entry.clone());
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
    // The last version deposited but possibly not yet consumed: promoted to `applied_src` once
    // the mailbox slot is observed empty again (the VM took it) — under the mailbox lock, so the
    // next diff is always against what actually runs.
    let mut deposited: Option<String> = None;

    loop {
        let event = match rx.recv() {
            Ok(event) => event,
            Err(_) => return,
        };
        if !relevant(&event) {
            continue;
        }
        let mut paths: Vec<std::path::PathBuf> = match &event {
            Ok(e) => e.paths.clone(),
            Err(_) => Vec::new(),
        };
        // Debounce, collecting every path the edit burst touched.
        while let Ok(more) = rx.recv_timeout(DEBOUNCE) {
            if relevant(&more)
                && let Ok(e) = &more
            {
                paths.extend(e.paths.iter().cloned());
            }
        }
        // Only source-relevant paths participate: an editor's atomic save emits a rename event
        // carrying BOTH paths (temp file + target), and the temp half must not read as "a change
        // outside the entry file".
        paths.retain(|p| relevant_path(p));
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
        let checked = noeta_check::check_all(&new_program);
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
            wake.notify_one();
            continue;
        }
        // Diff against the last CONSUMED version; the lock makes "was the previous deposit
        // taken?" exact, so a replaced deposit still diffs from what actually runs.
        let mut slot = match mailbox.plan.lock() {
            Ok(slot) => slot,
            Err(_) => return,
        };
        if slot.is_none()
            && let Some(consumed) = deposited.take()
        {
            applied_src = consumed;
        }
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
                *slot = Some(plan);
                deposited = Some(new_src);
                // A green deposit supersedes any pending red-check overlay, and the wake makes
                // an idle server apply it now rather than at its next request.
                if let Ok(mut err) = mailbox.error.lock() {
                    err.take();
                }
                wake.notify_one();
            }
            noeta_compiler::hotswap::SwapDiff::NeedsRestart(blockers) => {
                drop(slot);
                for b in &blockers {
                    eprintln!("[hot] restart needed: {b}");
                }
                std::process::exit(HOT_RESTART_CODE);
            }
        }
    }
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

    #[test]
    fn source_and_manifest_paths_are_relevant() {
        assert!(relevant_path(Path::new("src/main.noe")));
        assert!(relevant_path(Path::new("/abs/dir/app.noe")));
        assert!(relevant_path(Path::new("noeta.toml")));
        assert!(relevant_path(Path::new("deps/noeta.lock")));
    }

    #[test]
    fn hidden_dirs_backups_and_foreign_files_are_not() {
        assert!(!relevant_path(Path::new(".git/objects/ab.noe")));
        assert!(!relevant_path(Path::new("src/.cache/x.noe")));
        assert!(!relevant_path(Path::new("src/main.noe~")));
        assert!(!relevant_path(Path::new("src/lib.rs")));
        assert!(!relevant_path(Path::new("Cargo.toml")));
    }

    #[test]
    fn relative_prefixes_do_not_count_as_hidden() {
        assert!(relevant_path(Path::new("./src/main.noe")));
        assert!(relevant_path(Path::new("../sibling/app.noe")));
    }
}
