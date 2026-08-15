# noeta-host-real

The real host: real-disk IO, the real process environment, and a per-isolate async runtime.

- **Takes in:** the `noeta_stdlib::Host` and `Executor` capability traits.
- **Emits:** `RealHost` (real-disk fs + real `env`/`args`) and `RealExecutor` (a real `tokio` executor).

This is the non-sandbox side of the host/executor split. Where `SandboxHost`/`SandboxExecutor` (in `noeta-stdlib`) are the deterministic in-memory world the conformance differential always runs, `RealHost` and `RealExecutor` are what the CLI, REPL, and (later) server give a real program — real process environment/args and real-disk file IO. They are **never** used in the differential, so determinism is not their job; they are integration-tested outside the corpus, keeping `skipped == 0`.

Disk IO runs on a per-isolate `tokio` `current_thread` runtime, matching the shared-nothing isolate model (no work-stealing across heaps). The `Host` surface is still synchronous, so each IO method drives its future to completion with `block_on` at the leaf and returns a plain value. Building the IO path on `tokio` now means the `async`/`await` surface layered on later is additive (these `tokio::fs` calls get `await`ed instead of `block_on`-ed) rather than a rewrite. The filesystem is real-disk (paths relative to the process working directory) with a directory hierarchy mapping onto `tokio::fs`. This crate is `unsafe`-free.

## Interrupting a blocking leaf

Everything here that can wait indefinitely is a party to the run's cancellation. `Cancellable::set_cancel` hands the host the run's flag and its `CancelWake`; the host registers **one** hook, and that hook rouses every live party — a child's output buffer (`notify_all` under its own lock), a process exit slot, an open stream (a sentinel down its frame channel), and a request in flight (a `Notify` the fetch is `select!`ed against). The flag decides, the wake only rouses: a roused party re-reads the flag and returns `ErrorKind::Interrupted`, and the safepoint the unwind reaches is where the cancellation is actually honored.

The hard rule behind the shape is that **ending a wait is not ending the work**. A blocking leaf's body runs on a pool the run's teardown waits for, so a leaf that is abandoned rather than returned from turns a leaked thread into a hung run — which is why every interruptible leaf returns an error and none is dropped mid-flight, and why a `wait` on a cancellable run detaches its reap onto a background thread instead of blocking in `waitpid`, which nothing in this process can end early. File reads stay uninterruptible: they block in the operating system, and a file read that never returns is a FIFO or a character device rather than a file.

An unarmed host — every run that cannot be cancelled — takes none of this: the flag can never read true, so each leaf blocks exactly as it would without the seam.

## Streaming HTTP

The real host answers the `Network` capability's streaming seam two ways.

**Reading** a body incrementally (`std.http.client.stream`) gives each stream its own thread with its own runtime, which owns the request from `send()` to the last byte. That is forced rather than chosen: a reqwest response whose body is still arriving is tied to the runtime that drove `send()`, this host's runtime is `current_thread` and only runs inside `block_on`, and the `Network` seam gives a host no access to the *executor's* runtime — so a body opened here and read from there would stall silently. The response head comes back over that channel, so opening a stream still fails as a classified `NetError` — and so the caller can read the **status and headers** the server answered with before consuming a frame. That is the only moment they exist on this path: a non-2xx body is typically a JSON document, which an SSE reader correctly cuts into no frames at all, so a discarded head made a streamed `429` look exactly like an empty stream. Decoded frames then flow over a **bounded** channel, so a fast server against a slow reader gets backpressure through TCP rather than unbounded buffering. Dropping the receiving end (`close()`, or host teardown) ends the pump and releases the connection.

**Writing** one (`std.http.server.sse`) is HTTP/1.1 **chunked** with a flush per frame — chunked because each event must be independently flushable while the connection stays open, which close-delimited framing cannot offer an intermediary any reason to respect.

Both halves are exercised over real sockets in `crates/noeta-cli/tests/live_stream.rs` (`#[ignore]`d), which is the only place they can be: the conformance corpus always runs the sandbox.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
