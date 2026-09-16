//! Cancellation: making `notifications/cancelled` stop the *work*, not just the reply.
//!
//! rmcp spawns every request as its own task and hands the handler a [`CancellationToken`]. A
//! client's cancellation fires that token and makes rmcp drop the eventual response ("dropping
//! response for cancelled request"), which is what the protocol asks for. Nothing about it reaches
//! the tool body, so a tool that never looks at the token runs to completion with its answer thrown
//! away: the client sees a cancelled request, and this process keeps a core busy compiling or
//! executing for the rest of the run's budget.
//!
//! Two seams carry the token into the work, because there are two kinds of long tool:
//!
//! - **Compiler work** rides salsa, whose queries already poll for cancellation at every fetch and,
//!   inside the checker, once per top-level declaration. [`analyzing`] runs the query on a blocking
//!   thread, keeps a second database handle on the async side, and on cancellation calls
//!   [`LangDatabase::cancel_in_flight`] — the in-flight query unwinds mid-module and the thread is
//!   joined before the tool returns.
//! - **Program execution** rides the VM's per-instruction [`Debugger`](noeta_vm::Debugger) hook,
//!   the same seam the liveness limits use. The token is read there directly (see
//!   [`crate::execute`] and [`crate::debug`]).
//!
//! Every path here **joins its worker before returning**, so a cancelled request leaves no thread
//! running behind it and no client waiting: rmcp's response-dropping is the only thing the client
//! sees, and it sees it for a request whose work has already stopped. A panic in the worker is
//! re-raised on the async side rather than swallowed, so [`crate::catching_panics`] still turns it
//! into the JSON-RPC error a client can read.

use crate::analyze::Prepared;
use noeta_db::LangDatabase;
use rmcp::ErrorData;
use rmcp::handler::server::wrapper::Json;
use tokio_util::sync::CancellationToken;

/// The error a cancelled tool returns. rmcp drops the response for a request the client cancelled,
/// so this is rarely seen on the wire; it exists because the cancellation path must end in a value
/// like any other, and because an in-process caller (a test, an embedder) gets to see *why* the
/// tool stopped.
pub(crate) fn cancelled(tool: &str) -> ErrorData {
    ErrorData::internal_error(
        format!("the `{tool}` tool stopped: the request was cancelled"),
        None,
    )
}

/// Run one tool's compiler work on a blocking thread, abandoning it if the request is cancelled.
///
/// `work` gets the prepared workspace and produces the tool's output. On cancellation the salsa
/// queries in flight on `prepared`'s database unwind (`noeta_ide::catch_cancelled` absorbs that
/// unwind, and only that one), the worker thread is joined, and the tool reports [`cancelled`].
///
/// The work moves off the async runtime whether or not anything cancels it, which is where it
/// belonged anyway: a whole-workspace check is seconds of synchronous compilation, and running it
/// on a runtime worker blocks every other request in the session. The blocking pool inherits the
/// runtime's thread stack size, so the front end keeps the stack [`crate::run_stdio`] sizes for it.
pub(crate) async fn analyzing<T, F>(
    ct: CancellationToken,
    tool: &'static str,
    prepared: Prepared,
    work: F,
) -> Result<Json<T>, ErrorData>
where
    F: FnOnce(&Prepared) -> T + Send + 'static,
    T: Send + 'static,
{
    let canceller = prepared.db.clone();
    let worker =
        tokio::task::spawn_blocking(move || noeta_ide::catch_cancelled(move || work(&prepared)));
    match joined(ct, canceller, worker).await? {
        Some(value) => Ok(Json(value)),
        None => Err(cancelled(tool)),
    }
}

/// [`analyzing`], for a tool whose work is fallible before it produces an output.
pub(crate) async fn analyzing_fallible<T, F>(
    ct: CancellationToken,
    tool: &'static str,
    prepared: Prepared,
    work: F,
) -> Result<Json<T>, ErrorData>
where
    F: FnOnce(&Prepared) -> Result<T, ErrorData> + Send + 'static,
    T: Send + 'static,
{
    let canceller = prepared.db.clone();
    let worker =
        tokio::task::spawn_blocking(move || noeta_ide::catch_cancelled(move || work(&prepared)));
    match joined(ct, canceller, worker).await? {
        Some(value) => Ok(Json(value?)),
        None => Err(cancelled(tool)),
    }
}

/// Move blocking work off the async runtime and join it.
///
/// For work that reads the token through a seam of its own: the VM's per-instruction hook
/// (`eval`), or a sweep that polls between entries (`check`). It reports cancellation in its own
/// return value, so there is nothing to interpret here — the job of this function is that a tool
/// which takes seconds does not hold a runtime worker while it does, and that a panic in it still
/// reaches [`crate::catching_panics`].
pub(crate) async fn offload<T, F>(work: F) -> Result<T, ErrorData>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(rejoin_panic)
}

/// Wait for `worker`, cancelling its database's in-flight queries if `ct` fires first.
///
/// The cancel itself blocks — salsa's revision bump waits for the worker to unwind and drop its
/// handle — so it runs on the blocking pool too, and `worker` is awaited afterwards either way.
/// That ordering is what makes the tool's return mean "the work has stopped", rather than "the work
/// was asked to stop".
async fn joined<T>(
    ct: CancellationToken,
    mut canceller: LangDatabase,
    worker: tokio::task::JoinHandle<Option<T>>,
) -> Result<Option<T>, ErrorData>
where
    T: Send + 'static,
{
    tokio::pin!(worker);
    let joined = tokio::select! {
        done = &mut worker => done,
        () = ct.cancelled() => {
            // Flag the cancellation and wait for the unwind, then collect the (empty) result.
            let _ = tokio::task::spawn_blocking(move || canceller.cancel_in_flight()).await;
            worker.await
        }
    };
    joined.map_err(rejoin_panic)
}

/// Turn a worker's join failure back into the panic it was, on the async side.
///
/// A tool that panics must reach the client as a JSON-RPC error naming the tool — that is
/// [`crate::catching_panics`]'s whole job, and it can only see an unwind on its own side of the
/// join. Resuming here puts the original payload back in its path, so moving work onto a thread
/// changes nothing about what a client is told.
fn rejoin_panic(join: tokio::task::JoinError) -> ErrorData {
    if join.is_panic() {
        std::panic::resume_unwind(join.into_panic());
    }
    ErrorData::internal_error(
        format!("a tool's worker thread ended without a result: {join}"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A module big enough that its front end takes a measurable while in a test build.
    fn oversized() -> String {
        let mut text = String::new();
        for i in 0..4_000 {
            text.push_str(&format!(
                "fn f{i}(a: int, b: int): int {{\n  c = a + b + {i}\n  return c * 2\n}}\n"
            ));
        }
        text
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_analysis_abandons_the_compile() {
        noeta_stdlib::registry::default_seeded();
        let source = Some(oversized());

        // The control: how long the whole front end takes when nobody cancels it.
        let prepared = crate::analyze::prepare(&source, &None).expect("prepare");
        let start = Instant::now();
        let full = analyzing(
            CancellationToken::new(),
            "pipeline",
            prepared,
            crate::introspect::pipeline,
        )
        .await;
        let uncancelled = start.elapsed();
        assert!(full.is_ok(), "the control run must produce an answer");

        // The same work, withdrawn once it is under way.
        let prepared = crate::analyze::prepare(&source, &None).expect("prepare");
        let ct = CancellationToken::new();
        let fire = ct.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            fire.cancel();
        });
        let start = Instant::now();
        let out = analyzing(ct, "pipeline", prepared, crate::introspect::pipeline).await;
        let elapsed = start.elapsed();
        let err = out.err().expect("a cancelled analysis produces no answer");
        assert!(
            err.message.contains("cancelled"),
            "the error should say why: {}",
            err.message
        );
        assert!(
            elapsed * 2 < uncancelled,
            "the compile ran on after the cancel: {elapsed:?} against an uncancelled {uncancelled:?}"
        );
    }
}
