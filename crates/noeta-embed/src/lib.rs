//! **`noeta-embed`** — drive a live Noeta session from a host process (server-hmr E0–E2).
//!
//! The canonical consumer is a game engine's scripting layer: load a script, call its functions
//! from the frame loop, and hot-swap edited code into the running session **without losing
//! reactive state** — the same swap core `noeta serve --watch` uses, driven by the host's own
//! trigger (an asset-pipeline event, a file watcher, a debug key).
//!
//! ```no_run
//! use noeta_embed::{Session, Value};
//!
//! let mut session = Session::new(
//!     "use std.reactive.{signal}\n\
//!      score = signal(0)\n\
//!      fn update(dt: float): int {\n\
//!      \x20   score.update(fn(s) { return s + 1 })\n\
//!      \x20   return score.get()\n\
//!      }\n",
//! )
//! .unwrap();
//! let frames = session.call("update", &[Value::Float(0.016)]).unwrap();
//! assert_eq!(frames, Value::Int(1));
//! // … the developer edits `update` — swap it in; `score` (reactive state) survives:
//! // session.hot_swap(new_source)?;
//! ```
//!
//! # Stability: none, deliberately
//!
//! This crate is **unstable by decision** (2026-07-11): a 0.x surface that adapts to its
//! consumers until a real engine integration has exercised it. Expect breaking changes between
//! minor versions.
//!
//! # The value bridge
//!
//! [`Value`] is a deep-copy bridge: scalars, strings, lists, and string-keyed maps cross the
//! boundary by value in both directions (a Noeta object comes back as a [`Value::Map`] in
//! declared field order; a tuple/set as a [`Value::List`]).
//!
//! For values a host keeps **across frames** — a game engine's entity/object references — use
//! [`Handle`]s instead of marshalling: [`Session::call_keep`] retains the result and returns a
//! handle, which passes back into later calls as a [`Value::Handle`] with no copy, mutates in
//! place, and reads via [`Session::read`]. Handles are GC-rooted, so a value the host holds is
//! never reclaimed under it, and a handle the host forgets reclaims when the session drops.
//!
//! A `List<@packed>` entity buffer held as a handle and mutated through script functions covers
//! the engine's hot path without copying. Direct host-side raw-bytes read/write of a packed
//! buffer (bypassing script) is the remaining value-bridge growth point on this unstable surface.
//!
//! # Hosts and extensions
//!
//! [`Session::new`] runs on the deterministic sandbox host (in-memory fs, logical clock, seeded
//! randomness) — right for tests and for engines that expose their world through their own
//! extension instead. [`Session::builder`] swaps in the real host (real disk/env/network) or a
//! custom [`Host`](noeta_stdlib::Host) implementation. Native extensions register either **per
//! process** via [`install_extensions`] (the shared default registry — right for a host with one
//! fixed extension set) or **per session** via [`Builder::with_extensions`] (instance-registry IR5):
//! a session with its own assembled registry resolves native names — from type-check through runtime
//! dispatch — against *its* extensions, so two sessions in one process can run different sets. A
//! session's value heap is thread-local, so concurrent sessions run one-per-thread (like isolates).

use noeta_compiler::hotswap::{SwapDiff, diff_programs};
use noeta_stdlib::{NativeOut, NativeValue, Scalar};
use noeta_vm::{EmbedArg, EmbedHandle, SessionOutput, VmSession};

/// An opaque, GC-safe reference to a live Noeta value the host keeps across calls (server-hmr F3)
/// — a game engine's entity/object reference held between frames, never marshalled out. Mint one
/// with [`Session::call_keep`], read its current value with [`Session::read`], pass it back as a
/// [`Value::Handle`] argument (zero copy), and free it with [`Session::release`] (a forgotten
/// handle reclaims when the session drops).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle(EmbedHandle);

/// A value crossing the embed boundary, in either direction. Deep-copied at the seam.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    /// A string-keyed aggregate: a Noeta map (key order) or object (declared field order).
    Map(Vec<(String, Value)>),
    /// An opaque handle to a session-held value (server-hmr F3). Passing one as a call argument
    /// hands the live value back without a copy; it is never produced by a deep read (which
    /// materializes the value), only carried by the host.
    Handle(Handle),
}

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::Int(n)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Float(f)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}
impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Value {
        Value::List(items.into_iter().map(Into::into).collect())
    }
}

impl TryFrom<Value> for i64 {
    type Error = Error;
    fn try_from(v: Value) -> Result<i64, Error> {
        match v {
            Value::Int(n) => Ok(n),
            other => Err(Error::WrongType {
                expected: "int",
                got: format!("{other:?}"),
            }),
        }
    }
}
impl TryFrom<Value> for f64 {
    type Error = Error;
    fn try_from(v: Value) -> Result<f64, Error> {
        match v {
            Value::Float(f) => Ok(f),
            Value::Int(n) => Ok(n as f64),
            other => Err(Error::WrongType {
                expected: "float",
                got: format!("{other:?}"),
            }),
        }
    }
}
impl TryFrom<Value> for bool {
    type Error = Error;
    fn try_from(v: Value) -> Result<bool, Error> {
        match v {
            Value::Bool(b) => Ok(b),
            other => Err(Error::WrongType {
                expected: "bool",
                got: format!("{other:?}"),
            }),
        }
    }
}
impl TryFrom<Value> for String {
    type Error = Error;
    fn try_from(v: Value) -> Result<String, Error> {
        match v {
            Value::Str(s) => Ok(s),
            other => Err(Error::WrongType {
                expected: "string",
                got: format!("{other:?}"),
            }),
        }
    }
}

fn to_native_out(value: &Value) -> NativeOut {
    match value {
        Value::Unit => NativeOut::Unit,
        Value::Bool(b) => NativeOut::Scalar(Scalar::Bool(*b)),
        Value::Int(n) => NativeOut::Scalar(Scalar::Int(*n)),
        Value::Float(f) => NativeOut::Scalar(Scalar::Float(*f)),
        Value::Str(s) => NativeOut::Str(s.clone()),
        Value::List(items) => NativeOut::List(items.iter().map(to_native_out).collect()),
        Value::Map(entries) => NativeOut::Map(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), to_native_out(v)))
                .collect(),
        ),
        // A handle is a live-value reference, not marshallable data — it crosses as an
        // `EmbedArg::Handle` (see `to_embed_arg`), never through the value marshal.
        Value::Handle(_) => unreachable!("a handle argument is routed through `to_embed_arg`"),
    }
}

/// An argument to a call: a handle passes the live value back (zero copy), everything else
/// marshals by value (server-hmr F3).
fn to_embed_arg(value: &Value) -> EmbedArg {
    match value {
        Value::Handle(h) => EmbedArg::Handle(h.0),
        other => EmbedArg::Value(to_native_out(other)),
    }
}

fn from_native(value: NativeValue) -> Value {
    match value {
        NativeValue::Unit => Value::Unit,
        NativeValue::Scalar(Scalar::Bool(b)) => Value::Bool(b),
        NativeValue::Scalar(Scalar::Int(n)) => Value::Int(n),
        NativeValue::Scalar(Scalar::Float(f)) => Value::Float(f),
        NativeValue::Scalar(Scalar::F32(f)) => Value::Float(f as f64),
        NativeValue::Str(s) => Value::Str(s),
        NativeValue::List(items) => Value::List(items.into_iter().map(from_native).collect()),
        NativeValue::Map(entries) => Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| (k, from_native(v)))
                .collect(),
        ),
        // The deep view renders everything else as a display string (`<fn>`, a Uuid's canonical
        // form) — surface it as that string rather than inventing an opaque variant prematurely.
        NativeValue::Extern(e) => Value::Str(e.display_string()),
        NativeValue::Bytes(b) => Value::List(b.into_iter().map(|x| Value::Int(x as i64)).collect()),
        NativeValue::Object { fields, .. } => Value::List(
            fields
                .into_iter()
                .map(|s| from_native(NativeValue::Scalar(s)))
                .collect(),
        ),
        NativeValue::Opaque(name) => Value::Str(format!("<{name}>")),
        // An enum value: a fieldless/backed variant is fully described by its case name, so it
        // surfaces as that string (consistent with the display-string convention above). A
        // payload-carrying one surfaces as the single-entry map `{case: [payload…]}` — the same
        // shape `json.stringify` writes it as, for the same reason `Bytes` above surfaces as a list
        // of byte values: this door and the JSON door describe one value, so they describe it
        // identically. `Value` has no variant kind, so *some* spelling had to be chosen; dropping
        // the payload and keeping only the tag is the one choice that loses data.
        NativeValue::Variant {
            variant, fields, ..
        } => {
            if fields.is_empty() {
                Value::Str(variant)
            } else {
                Value::Map(vec![(
                    variant,
                    Value::List(fields.into_iter().map(from_native).collect()),
                )])
            }
        }
        // A native class instance (native-extensibility S2): surface its fields as a keyed map,
        // like an object/record aggregate.
        NativeValue::Instance { fields, .. } => Value::Map(
            fields
                .into_iter()
                .map(|(k, v)| (k, from_native(v)))
                .collect(),
        ),
    }
}

/// What went wrong at the embed boundary.
#[derive(Debug)]
pub enum Error {
    /// The source does not lex/parse; rendered diagnostics, one per entry.
    Parse(Vec<String>),
    /// The source does not type-check; rendered diagnostics.
    Check(Vec<String>),
    /// No top-level function of this name exists.
    NoSuchFunction(String),
    /// The call (or the load's top level) panicked: the message and any output so far.
    Panic { message: String, stdout: String },
    /// A [`TryFrom`] conversion mismatch.
    WrongType { expected: &'static str, got: String },
    /// The session's extension set does not assemble (duplicate identities, a type namespaced
    /// outside its unit's root, …) — the registry's validation message.
    Extension(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Parse(ds) => write!(f, "parse error: {}", ds.join("; ")),
            Error::Check(ds) => write!(f, "check error: {}", ds.join("; ")),
            Error::NoSuchFunction(name) => write!(f, "no function named `{name}`"),
            Error::Panic { message, .. } => write!(f, "panic: {message}"),
            Error::WrongType { expected, got } => {
                write!(f, "expected {expected}, got {got}")
            }
            Error::Extension(msg) => write!(f, "extension set does not assemble: {msg}"),
        }
    }
}
impl std::error::Error for Error {}

/// The outcome of a [`Session::hot_swap`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapOutcome {
    /// Nothing behavioral changed; the session is untouched.
    Unchanged,
    /// The edit swapped into the live session: what changed, and which reactive bindings were
    /// preserved (the HMR state rule — *reactive state survives edits; plain state re-runs*).
    Swapped {
        changed: Vec<String>,
        preserved: Vec<String>,
    },
    /// The live session cannot absorb this edit (layout/signature/namespace change) — the host
    /// decides: reload the world via a fresh [`Session::new`], or keep running the old code.
    NeedsRestart(Vec<String>),
}

/// Register native extension units for every session in this process — the engine's own API
/// surface (`impl Extension`) plus whatever packages it links. Must run **before** the first
/// session (the registry assembles once; see the module docs for the instance-scoping caveat).
pub fn install_extensions(units: Vec<&'static (dyn noeta_ext_abi::Extension + Sync)>) {
    noeta_stdlib::registry::install_with_extras(&units);
}

/// Which world a session's IO reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKind {
    /// The deterministic sandbox: in-memory fs, logical clock, seeded randomness. Default —
    /// right for tests, and for engines that expose their world through their own extension.
    #[default]
    Sandbox,
    /// The real host: real disk, env, network, wall clock.
    Real,
}

/// Configures a [`Session`] before load.
#[derive(Default)]
pub struct Builder {
    host: HostKind,
    /// This session's **own** extension units (instance-registry IR5). Empty ⇒ the session resolves
    /// native names against the process-global default (the same registry [`install_extensions`]
    /// seeds); non-empty ⇒ [`Builder::load`] assembles a private registry (std + these units) and
    /// threads it through the checker, the compiler, and the VM — so two sessions in one process can
    /// run different extension sets. See [`Builder::with_extensions`].
    extensions: Vec<&'static (dyn noeta_ext_abi::Extension + Sync)>,
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("host", &self.host)
            .field(
                "extensions",
                &self.extensions.iter().map(|e| e.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Builder {
    /// Select the host world ([`HostKind::Sandbox`] is the default).
    pub fn host(mut self, host: HostKind) -> Builder {
        self.host = host;
        self
    }

    /// Give **this session** its own extension set (instance-registry IR5), instead of the
    /// process-global one [`install_extensions`] seeds. The session's registry is `std` ∪ `units`,
    /// assembled privately and threaded through its checker, compiler, and VM — so a host can run two
    /// sessions with *different* extensions in the same process (a sandboxed plugin vs. a trusted
    /// one, an old vs. a new API surface). Repeated calls accumulate. Uniqueness still holds: a
    /// duplicate module identity across `units` (or vs. std) panics at [`Builder::load`], as at
    /// process-global install.
    pub fn with_extensions(
        mut self,
        units: Vec<&'static (dyn noeta_ext_abi::Extension + Sync)>,
    ) -> Builder {
        self.extensions.extend(units);
        self
    }

    /// Load `source` and run its top level; the session is live afterwards.
    pub fn load(self, source: &str) -> Result<Session, Error> {
        let program = parse(source)?;
        // A session with its own extension set resolves against a private registry threaded
        // through every stage; otherwise it rides the process-global default (`None`). IR5.
        // Assemblies are interned by unit set (`'static` is what the pipeline hands out, so the
        // registry must leak — interning bounds the leak by distinct configurations, not session
        // count), and a mis-assembled set is a proper `Err`, not a panic out of a library call.
        let registry: Option<&'static noeta_stdlib::registry::Registry> =
            if self.extensions.is_empty() {
                // The front-end no longer links the std units (audit-6 F2) — riding the
                // process-global default means this session's driver owns seeding it. An earlier
                // explicit `install_extensions` wins; after any install this is a no-op.
                noeta_stdlib::registry::default_seeded();
                None
            } else {
                Some(
                    noeta_stdlib::registry::interned_with_extras(&self.extensions)
                        .map_err(Error::Extension)?,
                )
            };
        let checked = match registry {
            Some(reg) => noeta_check::check_all_with_registry(&program, reg),
            None => noeta_check::check_all(&program),
        };
        if !checked.diagnostics.is_empty() {
            return Err(Error::Check(render_all(source, &checked.diagnostics)));
        }
        let (module, compiler) = match registry {
            Some(reg) => noeta_compiler::compile_with_sites_session_with_registry(
                &program,
                checked.sites,
                false,
                false,
                reg,
            ),
            None => {
                noeta_compiler::compile_with_sites_session(&program, checked.sites, false, false)
            }
        }
        .map_err(|u| Error::Check(vec![u.reason]))?;
        let host = self.host;
        let factory: noeta_vm::HostFactory = Box::new(move || match host {
            HostKind::Sandbox => (
                Box::new(noeta_stdlib::SandboxHost::new()),
                Box::new(noeta_stdlib::SandboxExecutor::new()),
            ),
            HostKind::Real => {
                let real_host = noeta_host_real::RealHost::new()
                    .expect("the real host requires a working runtime");
                let executor = noeta_host_real::RealExecutor::new()
                    .expect("the real executor requires a working runtime");
                (Box::new(real_host), Box::new(executor))
            }
        });
        let (session, out) = VmSession::adopted_with_registry(&module, compiler, factory, registry);
        if !out.trace.is_empty() {
            return Err(panic_error(out));
        }
        Ok(Session {
            session,
            source: source.to_string(),
            stdout: out.stdout,
            registry,
        })
    }
}

/// A live Noeta session a host process drives: load once, [`call`](Session::call) from the
/// host's own loop, [`hot_swap`](Session::hot_swap) edits in without losing reactive state.
#[derive(Debug)]
pub struct Session {
    session: VmSession,
    /// The source currently RUNNING — the hot-swap diff baseline.
    source: String,
    /// Output the program printed and the host has not yet drained ([`Session::take_stdout`]).
    stdout: String,
    /// This session's private registry (instance-registry IR5), when it was built with
    /// [`Builder::with_extensions`] — [`Session::hot_swap`] must check edits against the same
    /// registry the session type-checked and runs under, not the process-global default.
    /// `None` ⇒ the default registry.
    registry: Option<&'static noeta_stdlib::registry::Registry>,
}

impl Session {
    /// Load `source` on the default (sandbox) host and run its top level.
    pub fn new(source: &str) -> Result<Session, Error> {
        Session::builder().load(source)
    }

    /// Configure a session (host world) before loading.
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Call the top-level function `name` with `args`, returning its result. The session's state
    /// — globals, signals, everything — persists across calls; a panic inside the callee returns
    /// [`Error::Panic`] and the session survives.
    pub fn call(&mut self, name: &str, args: &[Value]) -> Result<Value, Error> {
        let embed_args: Vec<EmbedArg> = args.iter().map(to_embed_arg).collect();
        match self.session.call_mixed(name, embed_args) {
            Ok((value, out)) => {
                self.stdout.push_str(&out.stdout);
                Ok(from_native(value))
            }
            Err(noeta_vm::CallError::NoSuchFunction(name)) => Err(Error::NoSuchFunction(name)),
            Err(noeta_vm::CallError::Aborted(out)) => Err(panic_error(*out)),
        }
    }

    /// Call `name` and **keep** its result as a [`Handle`] the session holds live across frames
    /// (server-hmr F3) — no marshalling round-trip. Read its current value with [`Self::read`],
    /// pass it back as a `Value::Handle` argument, and free it with [`Self::release`].
    pub fn call_keep(&mut self, name: &str, args: &[Value]) -> Result<Handle, Error> {
        let embed_args: Vec<EmbedArg> = args.iter().map(to_embed_arg).collect();
        match self.session.call_retaining(name, embed_args) {
            Ok((handle, out)) => {
                self.stdout.push_str(&out.stdout);
                Ok(Handle(handle))
            }
            Err(noeta_vm::CallError::NoSuchFunction(name)) => Err(Error::NoSuchFunction(name)),
            Err(noeta_vm::CallError::Aborted(out)) => Err(panic_error(*out)),
        }
    }

    /// The current value behind a handle, materialized as a [`Value`] (server-hmr F3). Reading
    /// does not consume the handle.
    pub fn read(&mut self, handle: Handle) -> Value {
        from_native(self.session.read_handle(handle.0))
    }

    /// Free a handle (F3): drop the host's reference, destructor-aware. A handle the host forgets
    /// is reclaimed when the session drops.
    pub fn release(&mut self, handle: Handle) {
        self.session.release_handle(handle.0);
    }

    /// Swap `new_source` into the live session — the same core `noeta serve --watch` uses,
    /// driven by the host's own trigger. Transactional: a parse/check error leaves the session
    /// (and its baseline) untouched; [`SwapOutcome::NeedsRestart`] reports what only a reload
    /// can absorb and leaves the old code running.
    pub fn hot_swap(&mut self, new_source: &str) -> Result<SwapOutcome, Error> {
        let old_program = parse(&self.source)?;
        let new_program = parse(new_source)?;
        // Check against the SAME registry the session was loaded under (IR5): a session-private
        // native module/type must resolve during a swap exactly as it did at load.
        let checked = match self.registry {
            Some(reg) => noeta_check::check_all_with_registry(&new_program, reg),
            None => noeta_check::check_all(&new_program),
        };
        if !checked.diagnostics.is_empty() {
            return Err(Error::Check(render_all(new_source, &checked.diagnostics)));
        }
        match diff_programs(&old_program, &self.source, &new_program, new_source) {
            SwapDiff::Unchanged => {
                self.source = new_source.to_string();
                Ok(SwapOutcome::Unchanged)
            }
            SwapDiff::NeedsRestart(blockers) => Ok(SwapOutcome::NeedsRestart(
                blockers.iter().map(ToString::to_string).collect(),
            )),
            SwapDiff::Swap(plan) => {
                let out = self.session.hot_swap(&plan);
                if !out.trace.is_empty() {
                    // The fragment's re-run panicked; the program keeps running on what DID
                    // land (function bodies rebind before the top level re-runs).
                    return Err(panic_error(out));
                }
                self.stdout.push_str(&out.stdout);
                self.source = new_source.to_string();
                Ok(SwapOutcome::Swapped {
                    changed: plan.changed,
                    preserved: plan.preserved,
                })
            }
        }
    }

    /// Evaluate a source fragment against the live session (the REPL seam — a debug console's
    /// escape hatch; prefer [`call`](Session::call) on the hot path).
    pub fn eval(&mut self, fragment: &str) -> Result<Option<String>, Error> {
        let program = parse(fragment)?;
        let out = self.session.eval(&program);
        if !out.trace.is_empty() {
            return Err(panic_error(out));
        }
        self.stdout.push_str(&out.stdout);
        Ok(out.value)
    }

    /// Drain everything the program has printed since the last take.
    pub fn take_stdout(&mut self) -> String {
        std::mem::take(&mut self.stdout)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.session.teardown();
    }
}

fn parse(source: &str) -> Result<noeta_ast::Program, Error> {
    let src = noeta_span::Source::new(noeta_span::SourceId::FIRST, "<embed>", source);
    let lexed = noeta_lexer::lex(&src);
    let parsed = noeta_parser::parse(&src, &lexed.tokens);
    if !lexed.diagnostics.is_empty() || !parsed.diagnostics.is_empty() {
        let all: Vec<String> = lexed
            .diagnostics
            .iter()
            .chain(&parsed.diagnostics)
            .map(|d| d.message.clone())
            .collect();
        return Err(Error::Parse(all));
    }
    Ok(parsed.program)
}

fn render_all(_source: &str, diagnostics: &[noeta_diagnostics::Diagnostic]) -> Vec<String> {
    diagnostics.iter().map(|d| d.message.clone()).collect()
}

fn panic_error(out: SessionOutput) -> Error {
    let message = out
        .diagnostics
        .first()
        .map(|d| d.message.clone())
        .unwrap_or_else(|| "the program aborted".to_string());
    Error::Panic {
        message,
        stdout: out.stdout,
    }
}
