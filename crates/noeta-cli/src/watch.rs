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

    loop {
        let mut child = match Command::new(&exe).args(args).spawn() {
            Ok(child) => child,
            Err(e) => {
                eprintln!("[watch] failed to start: {e}");
                return ExitCode::FAILURE;
            }
        };
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
        eprintln!("[watch] change detected — restarting");
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
