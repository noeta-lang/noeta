//! The host-capability seam (M2.1) — the trait side.
//!
//! Real host IO (filesystem, environment, args, wall-clock, network) is non-deterministic and
//! would break the differential oracle. So every host-coupled effect both backends perform is
//! funneled through one [`Host`] trait with two intended implementations: `SandboxHost` — the
//! deterministic in-memory sandbox that conformance and `--differential` always run (in `noeta-
//! stdlib`, since it drives the concrete VFS/PRNG/net responder) — and a real host (real disk +
//! real `std::env` + reqwest, in `noeta-host-real`), constructed only by the CLI/REPL/server and
//! never differential-tested.
//!
//! Only the capability *traits* (and [`ReadSource`], the read-handle backing the [`FileReader`]
//! seam returns) live here in the ABI crate; the concrete `SandboxHost` and its sandbox constants
//! stay in `noeta-stdlib` next to the modules whose bytes it owns.

use crate::{ErrorKind, StdError};

/// How a read handle's bytes are delivered, decided by the host at `fs.open` time and handed to
/// `FileHandle::open_read`. Keeping this choice in one neutral enum is what lets the same handle
/// be eager on the deterministic sandbox and lazy on the real host without the handle knowing
/// which. (The `FileHandle` that streams over it lives in `noeta-stdlib`.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadSource {
    /// The entire file content, already in memory. The handle streams over it with no further host
    /// calls — deterministic, and byte-identical to the pre-P-LAZY snapshot behavior. The sandbox
    /// always uses this (its files are small in-memory fixtures).
    Snapshot(String),
    /// A host-side lazy reader identified by this id; the handle pulls more bytes via
    /// [`FileReader::fs_read_more`] as the cursor consumes them. Real-host only.
    Lazy(u64),
}

/// **Read-handle backing** (P-LAZY): how an opened file's bytes are delivered. `fs.open(path, "r")`
/// calls `fs_open_read` to learn whether they arrive as a deterministic whole-file
/// [`ReadSource::Snapshot`] (the sandbox) or a [`ReadSource::Lazy`] reader the handle pulls from via
/// `fs_read_more` (the real host, so a large file is never buffered whole). `fs_read_more` is only
/// ever called with an id this host returned in a `Lazy`, and returns the next chunk (valid UTF-8 —
/// a line at a time) or `None` at EOF.
///
/// This is the *narrowest* filesystem capability, split out so a read handle (`FileHandle`) — and a
/// read-only test double — depend on exactly these two methods rather than the whole [`Host`]. It is
/// a supertrait of [`FileSystem`].
pub trait FileReader {
    fn fs_open_read(&mut self, path: &str) -> Result<ReadSource, StdError>;
    fn fs_read_more(&mut self, id: u64) -> Result<Option<String>, StdError>;
}

/// **Filesystem** capability: whole-file/bytes reads and writes, existence/removal, listing, and the
/// directory hierarchy — plus read-handle backing via the [`FileReader`] supertrait. The methods
/// that touch storage are fallible so a real host can surface disk errors; the in-memory
/// `SandboxHost` simply never errors.
pub trait FileSystem: FileReader {
    fn fs_write(&mut self, path: &str, content: &str) -> Result<(), StdError>;
    fn fs_append(&mut self, path: &str, content: &str) -> Result<(), StdError>;
    fn fs_read(&self, path: &str) -> Result<String, StdError>;
    /// Write raw bytes (P-PACK 4.4 `fs.write_bytes`) — the binary counterpart of `fs_write`.
    fn fs_write_bytes(&mut self, path: &str, data: &[u8]) -> Result<(), StdError>;
    /// Read raw bytes (P-PACK 4.4 `fs.read_bytes`) — the binary counterpart of `fs_read`.
    fn fs_read_bytes(&self, path: &str) -> Result<Vec<u8>, StdError>;
    fn fs_exists(&self, path: &str) -> bool;
    fn fs_remove(&mut self, path: &str) -> Result<bool, StdError>;
    fn fs_list(&self) -> Result<Vec<String>, StdError>;

    // Directory hierarchy (M2.5). `fs_list_dir` returns a directory's immediate children (sorted);
    // `fs_mkdir` creates a directory and its ancestors; `fs_is_dir` reports whether a path is one.
    fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, StdError>;
    fn fs_mkdir(&mut self, path: &str) -> Result<(), StdError>;
    fn fs_is_dir(&self, path: &str) -> bool;
}

/// **Seeded PRNG** capability — the host owns the state; the SplitMix64 stepper stays pure.
pub trait Rng {
    fn rng_seed(&mut self, seed: i64);
    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError>;
    fn rng_float(&mut self) -> f64;
}

/// **Logical monotonic clock** capability — `monotonic` reads-then-advances; `sleep` advances without
/// blocking (deterministic, no wall-clock).
///
/// `clock_unix_ms` is the wall-time view (id-entropy U1): real `SystemTime` on the real host; on the
/// sandbox a **derived read** of the logical clock against the fixed sandbox epoch. It deliberately
/// does NOT advance the counter — a derived reading (a v7 UUID) must not perturb the user's
/// observable `monotonic` stream — but it advances under `sleep` like everything else, so
/// time-ordered ids still order deterministically.
pub trait Clock {
    fn clock_monotonic(&mut self) -> u64;
    fn clock_sleep(&mut self, ms: i64);
    fn clock_unix_ms(&mut self) -> u64;
}

/// **Entropy** capability (id-entropy U1) — raw random bits, distinct from [`Rng`] on purpose.
/// [`Rng`] is the *user-facing seeded* stream (`random.seed` rewinds it; every draw is observable
/// through `random.int`/`random.float`), so entropy consumers (UUID v4/v7) must not share it:
/// generating an id would perturb the user's `random` sequence, and `random.seed(42)` would rewind
/// ids. On the sandbox this is an independent fixed-seed SplitMix64 stream (deterministic, so the
/// differential can assert exact UUIDs); on the real host it is OS entropy.
pub trait Entropy {
    fn entropy_u64(&mut self) -> u64;
}

/// **Sequential ids** capability (id-entropy U2) — the counter behind `id.next_id()`: 1, 2, 3, ….
/// Host-owned so both backends share one dispatch (`next_id` agreement is by-construction, like
/// every registry module) and so REPL continuity rides the session's host. Deterministic on every
/// host — sequential ids are an ordering device, not entropy.
pub trait Ids {
    fn id_next(&mut self) -> u64;
}

/// **Network** capability (http arc H1) — outbound HTTP. The sandbox answers every request with a
/// deterministic pure responder (a pure function of the request, so the differential holds
/// regardless of URL); the real host performs it over the network. A transport failure (DNS,
/// connection, TLS) is a classified [`crate::NetError`]; an HTTP error *status* is an ordinary
/// response, not an error — that split is what makes `?` on a request mean "the network broke".
pub trait Network {
    fn net_fetch(
        &mut self,
        request: crate::NetRequest,
    ) -> Result<crate::NetResponse, crate::NetError>;

    /// Build the async work descriptor for `request` (http arc H3, the `http.*_async` surface).
    /// The dispatch tickets the returned descriptor on the executor. The default is a
    /// [`crate::net::NetFetchIo`] with no real body — it resolves through [`Self::net_fetch`] at
    /// spawn (deterministic in the sandbox; serial-but-correct on any host). `RealHost` overrides
    /// it to hand out a genuine reqwest future via [`crate::RealBody::Async`], for true
    /// concurrent fan-out. Kept off [`Self::net_fetch`] so the sandbox never touches a real body.
    fn net_spawn(&self, request: crate::NetRequest) -> Box<dyn crate::ExternIo> {
        Box::new(crate::net::NetFetchIo { request })
    }

    // --- Inbound: the server side (http-server S1). The exact inverse of the outbound side above:
    // the world initiates a connection and the program's handler responds. Determinism is the
    // mirror image of the pure responder — the sandbox drives a *pure, finite request script*
    // (`net_accept_next` pops it, then `None`), so a served program terminates in-oracle; the real
    // host binds a socket and blocks. ---

    /// Bind an inbound listener at `addr`, returning a listener id passed to [`Self::net_accept`]
    /// / [`Self::net_reply`]. The sandbox arms its deterministic request script (ignoring `addr`);
    /// the real host binds a real socket.
    fn net_listen(&mut self, addr: &str) -> Result<u64, StdError>;

    /// The next inbound connection for the default (sandbox / degraded) accept descriptor: a
    /// `(conn_id, request)`, or `None` once the listener is exhausted/closed. The sandbox pops its
    /// script; a real host overrides [`Self::net_accept`] with a genuine async accept and never
    /// reaches this (like `fs_read_more` on the sandbox).
    fn net_accept_next(
        &mut self,
        listener: u64,
    ) -> Result<Option<(u64, crate::NetRequest)>, StdError>;

    /// Build the async accept descriptor for `listener` — the inbound mirror of [`Self::net_spawn`].
    /// Default: an [`AcceptIo`](crate::net::AcceptIo) resolving through [`Self::net_accept_next`] at
    /// spawn (deterministic in the sandbox; serial on any host). `RealHost` overrides it with a
    /// `TcpListener::accept().await` future so a slow handler yields cooperatively while the next
    /// connection is awaited.
    fn net_accept(&self, listener: u64) -> Box<dyn crate::ExternIo> {
        Box::new(crate::net::AcceptIo { listener })
    }

    /// Write `response` to connection `conn` and close it, for the default (sandbox / degraded)
    /// reply descriptor. The sandbox records it (its request script is a pure driver); a real host
    /// overrides [`Self::net_reply`].
    fn net_reply_now(&mut self, conn: u64, response: crate::NetResponse) -> Result<(), StdError>;

    /// Build the async reply descriptor for `conn`. Default: a [`ReplyIo`](crate::net::ReplyIo)
    /// via [`Self::net_reply_now`]. `RealHost` overrides it with an async socket write on the
    /// executor's runtime, so a connection's IO stays on the runtime that accepted it.
    fn net_reply(&self, conn: u64, response: crate::NetResponse) -> Box<dyn crate::ExternIo> {
        Box::new(crate::net::ReplyIo {
            conn,
            response: Some(response),
        })
    }

    // --- Websocket hijack (server-hmr L0): an accepted connection upgrades from
    // one-reply-and-close to a persistent bidirectional TEXT-message stream — the transport under
    // LiveView's diff-push and the HMR client events. Determinism mirrors the request script: the
    // sandbox drives a fixed per-connection client conversation and records sends; the real host
    // overrides the descriptor builders with a genuine RFC 6455 handshake + frame codec. The
    // `*_now` defaults error, so a host that never serves websockets (WASI, browser) stays
    // compiling and a program reaching the surface gets an honest capability error.

    /// Switch `conn` to websocket mode, `key` being the client's `Sec-WebSocket-Key` (the sandbox
    /// arms its scripted conversation and ignores the key; the real host writes the 101 response).
    fn net_ws_upgrade_now(&mut self, _conn: u64, _key: &str) -> Result<(), StdError> {
        Err(StdError {
            kind: ErrorKind::Io,
            message: "this host does not serve websockets".to_string(),
        })
    }

    /// The next inbound text message on `conn` for the default recv descriptor — `None` once the
    /// peer closed (sandbox: the scripted conversation is exhausted).
    fn net_ws_recv_next(&mut self, _conn: u64) -> Result<Option<String>, StdError> {
        Err(StdError {
            kind: ErrorKind::Io,
            message: "this host does not serve websockets".to_string(),
        })
    }

    /// Write a text frame on `conn` for the default send descriptor (sandbox: recorded).
    fn net_ws_send_now(&mut self, _conn: u64, _text: &str) -> Result<(), StdError> {
        Err(StdError {
            kind: ErrorKind::Io,
            message: "this host does not serve websockets".to_string(),
        })
    }

    /// Close `conn`'s websocket (sandbox: drops the conversation state).
    fn net_ws_close_now(&mut self, _conn: u64) -> Result<(), StdError> {
        Err(StdError {
            kind: ErrorKind::Io,
            message: "this host does not serve websockets".to_string(),
        })
    }

    /// Build the upgrade descriptor. Default resolves through [`Self::net_ws_upgrade_now`].
    fn net_ws_upgrade(&self, conn: u64, key: String) -> Box<dyn crate::ExternIo> {
        Box::new(crate::net::WsUpgradeIo {
            conn,
            key: Some(key),
        })
    }

    /// Build the recv descriptor — resolves to `?string`, `None` = closed. Default via
    /// [`Self::net_ws_recv_next`]; `RealHost` overrides with an async frame read.
    fn net_ws_recv(&self, conn: u64) -> Box<dyn crate::ExternIo> {
        Box::new(crate::net::WsRecvIo { conn })
    }

    /// Build the send descriptor. Default via [`Self::net_ws_send_now`]; `RealHost` overrides
    /// with an async frame write.
    fn net_ws_send(&self, conn: u64, text: String) -> Box<dyn crate::ExternIo> {
        Box::new(crate::net::WsSendIo {
            conn,
            text: Some(text),
        })
    }

    /// Build the close descriptor. Default via [`Self::net_ws_close_now`].
    fn net_ws_close(&self, conn: u64) -> Box<dyn crate::ExternIo> {
        Box::new(crate::net::WsCloseIo { conn })
    }
}

/// **Host introspection** capability (M2.2). `env_keys` is sorted. The sandbox presents a fixed
/// fixture; a real host reads the real environment/args.
///
/// `env_set` (stdlib-gaps) writes into the **program's view** of the environment, not the real
/// process environment: the sandbox mutates its fixture map, and `RealHost` keeps a thread-safe
/// overlay consulted before the real environment (`std::env::set_var` is unsafe with live
/// threads, and isolates are OS threads). Reads observe writes; the parent process is untouched.
pub trait Env {
    fn env_get(&self, key: &str) -> Option<String>;
    fn env_set(&mut self, key: &str, value: &str);
    fn env_keys(&self) -> Vec<String>;
    fn args(&self) -> Vec<String>;
}

/// Which of the program's three standard streams a query refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdin,
    Stdout,
    Stderr,
}

/// **Console** capability (CLI-completion slice 2) — the program's standard input and terminal-ness.
/// A host effect (fixture in the sandbox, real I/O on `RealHost`), so the backend differential stays
/// deterministic — the mirror of [`Env`]: program *output* is batch-captured and compared, but stdin
/// and TTY-ness are ambient effects that belong on a capability, not in the compared buffers.
///
/// The sandbox presents a scripted stdin fixture and reports every stream non-interactive
/// (`is_tty` = false), so a read-loop program terminates in-oracle and both backends agree by
/// construction. `RealHost` reads real stdin and probes the real terminals.
pub trait Console {
    /// Read the next line of stdin (without the trailing newline), or `None` at EOF.
    fn stdin_read_line(&mut self) -> Option<String>;
    /// Read all remaining stdin to EOF as one string.
    fn stdin_read_all(&mut self) -> String;
    /// Whether `stream` is connected to an interactive terminal.
    fn is_tty(&self, stream: Stream) -> bool;
    /// Write `msg` to the real terminal IMMEDIATELY (bypassing the batch output buffer) and read
    /// one line of response — the single interactive path that survives batch-captured output.
    /// `None` at EOF. In the sandbox this is deterministic: it returns the next scripted stdin line
    /// and does not write anywhere observable.
    fn prompt(&mut self, msg: &str) -> Option<String>;
}

/// **Operating system** capability (stdlib-gaps) — process execution and system introspection,
/// the effect seam behind `std.os`. The introspection leaves are fixed fixtures in the sandbox
/// (`"sandbox"`/`"wasm32"`-style constants, pid 1, cwd `/`) and real values on `RealHost`;
/// `os_exec` is a **scripted in-oracle interpreter** of a tiny fixed command set in the sandbox
/// (deterministic — see `SandboxHost`) and a real `std::process::Command` on `RealHost`
/// (CLI-only, like the rest of the real host). `os.exit` does not live here: terminating the
/// *program* is interpreter control flow ([`crate::ErrorKind::Exit`]), not a host effect.
pub trait Os {
    /// The OS family name (`"linux"`, `"macos"`, `"windows"`, …; `"sandbox"` in the sandbox).
    fn os_platform(&self) -> String;
    /// The CPU architecture (`"x86_64"`, `"aarch64"`, …).
    fn os_arch(&self) -> String;
    /// The machine's hostname.
    fn os_hostname(&self) -> String;
    /// The number of logical CPUs available (≥ 1).
    fn os_cpus(&self) -> i64;
    /// The current working directory.
    fn os_cwd(&self) -> String;
    /// The process id.
    fn os_pid(&self) -> i64;
    /// Run `command` with `args` (verbatim — no shell), wait for it, and capture the outcome.
    /// A command that cannot be started at all (not found, not executable) is an `Io` error;
    /// a command that starts and fails is a successful `ExecResult` with its non-zero status.
    fn os_exec(
        &mut self,
        command: &str,
        args: &[String],
    ) -> Result<crate::os::ExecResult, StdError>;
    /// Build the async exec descriptor. Default: a [`crate::os::ExecIo`] resolving through
    /// [`Self::os_exec`] at spawn (deterministic in the sandbox); `RealHost` overrides it with
    /// a blocking-pool body so `exec_async` genuinely overlaps.
    fn os_exec_spawn(&self, command: String, args: Vec<String>) -> Box<dyn crate::ExternIo> {
        Box::new(crate::os::ExecIo { command, args })
    }

    // --- Process lifecycle (process-handle arc): spawn-and-hold, unlike the run-to-completion
    // `os_exec`. `os_spawn` starts a child and returns an opaque handle id; the program then
    // controls it through the [`crate::os::Process`] extern type, whose methods route back here by
    // id — the listener/reader-registry model, not `FileHandle`'s self-contained state. The
    // sandbox scripts a deterministic instant-complete child (in-oracle); `RealHost` holds a real
    // `std::process::Child` with drained pipes (CLI-only). ---

    /// Spawn `command` with `args` (verbatim — no shell) **without waiting**, returning an opaque
    /// handle id. The child runs concurrently with the program. A command that cannot be started
    /// at all (not found, not executable) is an `Io` error, exactly like [`Self::os_exec`].
    fn os_spawn(&mut self, command: &str, args: &[String]) -> Result<u64, StdError>;

    /// The OS process id of a spawned child, or `None` if `handle` is not a live handle.
    fn os_proc_pid(&self, handle: u64) -> Option<i64>;

    /// Block until the child exits and return its outcome (exit status + captured output).
    /// Idempotent: after the child is reaped the cached outcome is returned, so `wait` after a
    /// `try_wait` that already observed exit still works.
    fn os_proc_wait(&mut self, handle: u64) -> Result<crate::os::ExecResult, StdError>;

    /// Non-blocking poll: `Some(outcome)` if the child has exited (reaping it), `None` if it is
    /// still running. Lets a program supervise a child without blocking.
    fn os_proc_try_wait(&mut self, handle: u64) -> Result<Option<crate::os::ExecResult>, StdError>;

    /// Terminate the child (a forceful kill — SIGKILL / `TerminateProcess`). Idempotent: killing an
    /// already-exited child is `Ok`. A later `wait` observes the killed status.
    fn os_proc_kill(&mut self, handle: u64) -> Result<(), StdError>;

    /// Send `signal` to the child — the general form of [`Self::os_proc_kill`] (`kill` is exactly
    /// `signal(Signal::Kill)`, kept separate for its portable forceful-terminate guarantee).
    /// Idempotent: signalling an already-exited child is `Ok`. On non-Unix hosts only
    /// `Kill`/`Term` are expressible (as a forceful terminate); other signals are an `Io` error.
    fn os_proc_signal(&mut self, handle: u64, signal: crate::os::Signal) -> Result<(), StdError>;

    /// Build the `wait_async` work descriptor for a spawned child — the awaitable twin of
    /// [`Self::os_proc_wait`]. Default: a [`crate::os::ProcWaitIo`] resolving through
    /// [`Self::os_proc_wait`] at spawn (deterministic in the sandbox — the scripted child is already
    /// complete); `RealHost` overrides it with a blocking-pool body so the wait genuinely overlaps
    /// the isolate's other tasks. The exec-side [`Self::os_exec_spawn`] analogue for a held handle.
    fn os_proc_wait_spawn(&mut self, handle: u64) -> Box<dyn crate::ExternIo> {
        Box::new(crate::os::ProcWaitIo { handle })
    }

    // --- Streaming (process-streaming arc): read a child's stdout line-by-line *while it runs*,
    // and feed its stdin — unlike `wait`, which only hands back the fully-captured output at exit.
    // The real host keeps draining both pipes on background threads (so a chatty child never
    // deadlocks), and `read_line` consumes the stdout buffer through a per-handle cursor; the
    // sandbox streams its scripted output line by line. `wait` still returns the *whole* captured
    // output regardless of what was streamed. ---

    /// The next line of the child's stdout (without its trailing newline), advancing a per-handle
    /// read cursor. Blocks until a full line is available or the stream ends; `None` at end of
    /// output. A final unterminated line is returned once, then `None` (like `fs` `read_line`).
    fn os_proc_read_line(&mut self, handle: u64) -> Result<Option<String>, StdError>;

    /// Up to `count` **characters** from the child's stdout, advancing the same cursor as
    /// `read_line` — the not-necessarily-line-oriented read (POSIX `read` shape). Blocks only until
    /// at least one character is available, then returns up to `count` of them; `None` at end of
    /// output. `count <= 0` yields the empty string without consuming input.
    fn os_proc_read(&mut self, handle: u64, count: i64) -> Result<Option<String>, StdError>;

    /// The next line of the child's **stderr**, on its own independent cursor — the stderr twin of
    /// [`Self::os_proc_read_line`]. `wait` still returns the whole captured stderr.
    fn os_proc_read_stderr_line(&mut self, handle: u64) -> Result<Option<String>, StdError>;

    /// Write `data` to the child's stdin. An error if the child has no stdin or it is closed.
    fn os_proc_write_stdin(&mut self, handle: u64, data: &str) -> Result<(), StdError>;

    /// Close the child's stdin, signalling end-of-input to it (a program reading until EOF then
    /// unblocks). Idempotent.
    fn os_proc_close_stdin(&mut self, handle: u64) -> Result<(), StdError>;
}

/// **Peer-to-peer sync** capability (p2p P1) — the local-first stack's transport seam (§9.15). A
/// program `publish`es a message to a **topic** and `poll`s a topic for a message another peer
/// published; the messages are opaque bytes (p2panda, the eventual real backer, is deliberately
/// data-type-agnostic — CRDT state serializes to bytes and rides this seam in a later slice).
///
/// Determinism is the mirror of [`Network`]'s inbound side: the sandbox is a **deterministic
/// in-process broker** — `publish` enqueues to a per-topic FIFO, `poll` dequeues, `None` once a
/// topic drains — so a program that publishes then receives terminates in-oracle and both backends
/// agree. A real host would replace the broker with the p2panda gossip network (P3), non-
/// deterministic and CLI-only; until then it uses the same in-process broker (single-node loopback,
/// no real transport — cross-node/cross-isolate delivery is a later slice, like the HTTP server's
/// multi-core story).
pub trait P2p {
    /// Publish `message` to `topic` — enqueue it for delivery to peers subscribed to the topic
    /// (in the sandbox/loopback broker, to this node's own topic queue).
    fn p2p_publish(&mut self, topic: &str, message: Vec<u8>) -> Result<(), StdError>;

    /// The next pending message on `topic`, or `None` if none is available — the non-blocking leaf
    /// the async `p2p.receive` descriptor ([`crate::P2pReceiveIo`]) resolves through (a deterministic
    /// FIFO on the loopback broker; the real node's try-recv off its gossip channel), the same
    /// "serial degradation for free" the fs/net leaves rely on.
    fn p2p_poll(&mut self, topic: &str) -> Result<Option<Vec<u8>>, StdError>;

    /// Subscribe to `topic`, returning a subscription id whose cursor starts at the beginning of
    /// the topic log (p2p P2). Unlike the topic-level [`Self::p2p_poll`] (one implicit reader),
    /// each subscription has an **independent** cursor — genuine broadcast, so several replicas on
    /// one topic each receive every message. A `synced_signal` holds one for its topic.
    ///
    /// Fallible: the sandbox broker never errors (`Ok` always), but a real transport can fail to
    /// join a topic overlay (p2p P3), and that must reach the program rather than be swallowed.
    fn p2p_subscribe(&mut self, topic: &str) -> Result<u64, StdError>;

    /// The next message for subscription `sub` (advancing only its cursor), or `None` once it has
    /// caught up — what `synced_signal.sync()` drains to merge peers' states. Shared by ephemeral
    /// and durable subscriptions (both deliver bytes; the id namespace is one).
    fn p2p_poll_sub(&mut self, sub: u64) -> Result<Option<Vec<u8>>, StdError>;

    // --- Durable variants (p2p P3.2): eventual-consistency delivery ---------------------------
    //
    // `synced_signal` uses these so replicas **converge even after being offline** — a peer that
    // joins or reconnects later still receives everything published to the topic, not just what
    // arrives while it happens to be subscribed. The **default** delegates to the ephemeral
    // methods, which is exactly right for the sandbox: its broker is an append-only log with a
    // cursor from the start, so every subscriber already catches up. Only `RealHost` overrides
    // these — ephemeral maps to gossip (best-effort), durable to p2panda's log-sync protocol
    // (append-log + catch-up), which is the whole reason for the split.

    /// Durable publish (see above). Default: the ephemeral [`Self::p2p_publish`].
    fn p2p_publish_durable(&mut self, topic: &str, message: Vec<u8>) -> Result<(), StdError> {
        self.p2p_publish(topic, message)
    }

    /// Durable subscribe (see above). Default: the ephemeral [`Self::p2p_subscribe`]; the id is
    /// polled through the same [`Self::p2p_poll_sub`].
    fn p2p_subscribe_durable(&mut self, topic: &str) -> Result<u64, StdError> {
        self.p2p_subscribe(topic)
    }

    // --- Encrypted groups (p2p P3.4b): end-to-end-encrypted synced_signal ---------------------
    //
    // An encrypted `synced_signal(initial, topic, members)` routes through these instead of the
    // plaintext durable methods. The **default** is a transparent pass-through to the durable
    // transport: correct for the deterministic sandbox, where there are no real peers to hide state
    // from and encryption must not perturb the converged value — so an encrypted program stays
    // oracle-identical to its plaintext twin. Only `RealHost` under `ring-p2p` overrides them, where
    // the bytes are encrypted to the declared member set through a p2panda-spaces group.

    /// Open an encrypted group on `topic` for exactly `members` (peer-id hex strings), returning a
    /// subscription id polled through [`Self::p2p_group_poll`]. Default: a durable subscribe (the
    /// membership is irrelevant to the pass-through sandbox).
    fn p2p_group_open(&mut self, topic: &str, _members: &[String]) -> Result<u64, StdError> {
        self.p2p_subscribe_durable(topic)
    }

    /// Publish `plaintext` to the encrypted group on `topic` — encrypted to the member set on a
    /// real host. Default: a durable publish of the bytes unchanged.
    fn p2p_group_publish(&mut self, topic: &str, plaintext: Vec<u8>) -> Result<(), StdError> {
        self.p2p_publish_durable(topic, plaintext)
    }

    /// The next **decrypted** application payload for group subscription `sub`, or `None`. On a real
    /// host this drains control messages (membership / key material) as a side effect — welcoming
    /// declared members as their key bundles arrive — and returns only decrypted application state.
    /// Default: the plaintext [`Self::p2p_poll_sub`].
    fn p2p_group_poll(&mut self, sub: u64) -> Result<Option<Vec<u8>>, StdError> {
        self.p2p_poll_sub(sub)
    }

    /// Add `member` (peer-id hex) to the encrypted group on `topic` at runtime. On a real host the
    /// group creator welcomes it. Default: a no-op — the pass-through sandbox has no membership to
    /// enforce (the decrypted value is the same regardless of who is "in").
    fn p2p_group_add(&mut self, _topic: &str, _member: &str) -> Result<(), StdError> {
        Ok(())
    }

    /// Remove `member` (peer-id hex) from the encrypted group on `topic` at runtime, rotating the
    /// group key on a real host so it can no longer decrypt new state (revocation). Default: a no-op
    /// — the sandbox has no real crypto to revoke and its converged value is unaffected.
    fn p2p_group_remove(&mut self, _topic: &str, _member: &str) -> Result<(), StdError> {
        Ok(())
    }

    // --- Identity & status (p2p P3.3) ---------------------------------------------------------
    //
    // Both are meaningful only once there is a *real* network with a persistent identity to have
    // and a network to be offline from — so the sandbox/loopback broker keeps the trivial defaults
    // (no stable identity; "always synced", since a single-node broker never lags). Only `RealHost`
    // under `ring-p2p` overrides them from its live p2panda node.

    /// This node's stable identity — the hex-encoded Ed25519 public key it signs operations with,
    /// persisted across restarts (p2p P3.3). `None` on the loopback broker, which has no identity.
    fn p2p_identity(&mut self) -> Result<Option<String>, StdError> {
        Ok(None)
    }

    /// The synchronization state of `topic` from this node's point of view (p2p P3.3): whether it
    /// has caught up with peers ([`SyncStatus::Synced`]), is actively syncing, or has no live peer
    /// ([`SyncStatus::Offline`]). Default [`SyncStatus::Synced`] — the loopback broker is a single
    /// node with nothing to lag behind.
    fn p2p_sync_status(&mut self, _topic: &str) -> SyncStatus {
        SyncStatus::Synced
    }
}

/// Configuration for **real** peer networking on a host (para-namespace follow-on F2b). A host that
/// permits a live transport (`RealHost` — real IO, nondeterminism) carries one; the deterministic
/// sandbox and the WASI/browser hosts do not. It holds only the *policy* the extension needs, not any
/// transport: the p2panda node itself lives in the `para.p2p` extension, never in a host.
#[derive(Debug, Clone, Default)]
pub struct RealP2pConfig {
    /// The app-namespace that keys this node's persistent identity + on-disk store, set by the CLI
    /// via `RealHost::with_p2p_app`. `None` uses the transport's own default location.
    pub app_id: Option<String>,
}

/// Whether a host permits **real** peer networking (para-namespace arc → F2b). `P2p` used to be a
/// mandatory arm of the [`Host`] union; the p2p/local-first stack left `std` for the non-default
/// `para` package, and F2b moved the transport *impl* out of every host into the `para.p2p`
/// extension. So a host no longer provides `P2p` at all — it only declares, through this seam,
/// whether real networking is allowed and with what config. A real host returns `Some`; the
/// deterministic sandbox and the minimal hosts keep the default `None`, which the extension reads as
/// "use the loopback broker" (oracle-safe). The `P2p` impls now live entirely on the extension side
/// (the loopback [`crate::P2pBroker`] here, the real node in the out-of-tree para-p2p package).
pub trait P2pProvider {
    /// The real-networking config for this host, or `None` (the default) to use the deterministic
    /// loopback broker. Only a real host overrides it.
    fn real_p2p(&self) -> Option<RealP2pConfig> {
        None
    }
}

/// A `synced_signal`'s convergence state relative to its peers (p2p P3.3). Meaningless on the
/// deterministic loopback broker (always [`SyncStatus::Synced`]); real once a network can be
/// partitioned — a live p2panda node reports it from its log-sync session lifecycle, letting a
/// program render "working offline" / "syncing…" / "up to date".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// No live sync session with any peer — either none has been reached yet, or the last one
    /// failed/ended with no peer. The local state is authoritative but possibly stale.
    Offline,
    /// A sync session is in progress: exchanging or replaying a peer's log, not yet caught up.
    Syncing,
    /// Caught up with the peers reached — all their prior operations are merged (and, in live mode,
    /// new ones stream in). The convergence guarantee is met for those peers.
    Synced,
}

impl SyncStatus {
    /// The lowercase word `synced_signal.status()` surfaces to a program (`"offline"` / `"syncing"`
    /// / `"synced"`) — a plain string so a script matches it without importing an enum type.
    pub fn as_str(self) -> &'static str {
        match self {
            SyncStatus::Offline => "offline",
            SyncStatus::Syncing => "syncing",
            SyncStatus::Synced => "synced",
        }
    }
}

/// Every host-coupled effect the interpreters perform, behind one swappable seam — the union of the
/// core capability traits ([`FileSystem`], [`Rng`], [`Clock`], [`Env`], [`Os`], [`Entropy`],
/// [`Ids`], [`Network`], the three telemetry signals [`Tracing`](crate::Tracing) /
/// [`Metrics`](crate::Metrics) / [`Logging`](crate::Logging)) plus [`P2pProvider`], through which a
/// host **optionally** offers the [`P2p`] capability (the p2p/local-first stack left `std` for the
/// non-default `para` package, so peer networking is no longer a mandatory arm — see [`P2pProvider`]).
/// Backends hold a `Box<dyn Host>` and reach any capability through it; a consumer that needs only
/// one (a read handle → [`FileReader`], the RNG dispatch → [`Rng`], …) depends on that trait instead,
/// so a partial host (e.g. a read-only test double) implements exactly what it supports rather than
/// stubbing the rest.
///
/// Object-safe on purpose (IO is never a hot path, so the dynamic dispatch is immaterial). The
/// blanket impl means any type providing all the core capabilities *is* a `Host` automatically — a
/// host that omits `P2p` supplies a default `P2pProvider` (which returns `None`) and nothing else.
/// Splitting telemetry into three sibling traits costs nothing at runtime: a `dyn Host` has one
/// vtable and supertrait methods fold into it, so a call is one indirection regardless of which
/// sub-trait declared it.
pub trait Host:
    FileSystem
    + Rng
    + Clock
    + Env
    + Console
    + Os
    + Entropy
    + Ids
    + Network
    + P2pProvider
    + crate::Tracing
    + crate::Metrics
    + crate::Logging
{
}
impl<
    T: FileSystem
        + Rng
        + Clock
        + Env
        + Console
        + Os
        + Entropy
        + Ids
        + Network
        + P2pProvider
        + crate::Tracing
        + crate::Metrics
        + crate::Logging,
> Host for T
{
}
