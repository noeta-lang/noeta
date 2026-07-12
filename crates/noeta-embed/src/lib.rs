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
//! declared field order; a tuple/set as a [`Value::List`]). Opaque handles (keep a live Noeta
//! value across frames without copying) and zero-copy `@packed` buffers are growth points this
//! unstable surface expects to grow — see the arc plan.
//!
//! # Hosts and extensions
//!
//! [`Session::new`] runs on the deterministic sandbox host (in-memory fs, logical clock, seeded
//! randomness) — right for tests and for engines that expose their world through their own
//! extension instead. [`Session::builder`] swaps in the real host (real disk/env/network) or a
//! custom [`Host`](noeta_stdlib::Host) implementation. Native extensions register
//! **per process** via [`install_extensions`] (the registry is process-global; instance-scoped
//! registries are a known growth point).

use noeta_compiler::hotswap::{SwapDiff, diff_programs};
use noeta_stdlib::{NativeOut, NativeValue, Scalar};
use noeta_vm::{SessionOutput, VmSession};

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
pub fn install_extensions(units: Vec<&'static (dyn noeta_native::Extension + Sync)>) {
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
#[derive(Debug, Default)]
pub struct Builder {
    host: HostKind,
}

impl Builder {
    /// Select the host world ([`HostKind::Sandbox`] is the default).
    pub fn host(mut self, host: HostKind) -> Builder {
        self.host = host;
        self
    }

    /// Load `source` and run its top level; the session is live afterwards.
    pub fn load(self, source: &str) -> Result<Session, Error> {
        let program = parse(source)?;
        let checked = noeta_check::check_all(&program);
        if !checked.diagnostics.is_empty() {
            return Err(Error::Check(render_all(source, &checked.diagnostics)));
        }
        let (module, compiler) =
            noeta_compiler::compile_with_sites_session(&program, checked.sites, false, false)
                .map_err(|u| Error::Check(vec![u.reason]))?;
        let host = self.host;
        let factory: noeta_vm::HostFactory = Box::new(move || match host {
            HostKind::Sandbox => (
                Box::new(noeta_stdlib::SandboxHost::new()),
                Box::new(noeta_stdlib::SandboxExecutor::new()),
            ),
            HostKind::Real => {
                let real_host = noeta_runtime::RealHost::new()
                    .expect("the real host requires a working runtime");
                let executor = noeta_runtime::RealExecutor::new()
                    .expect("the real executor requires a working runtime");
                (Box::new(real_host), Box::new(executor))
            }
        });
        let (session, out) = VmSession::adopted(&module, compiler, factory);
        if !out.trace.is_empty() {
            return Err(panic_error(out));
        }
        Ok(Session {
            session,
            source: source.to_string(),
            stdout: out.stdout,
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
        let native_args: Vec<NativeOut> = args.iter().map(to_native_out).collect();
        match self.session.call_by_name(name, native_args) {
            Ok((value, out)) => {
                self.stdout.push_str(&out.stdout);
                Ok(from_native(value))
            }
            Err(noeta_vm::CallError::NoSuchFunction(name)) => Err(Error::NoSuchFunction(name)),
            Err(noeta_vm::CallError::Aborted(out)) => Err(panic_error(*out)),
        }
    }

    /// Swap `new_source` into the live session — the same core `noeta serve --watch` uses,
    /// driven by the host's own trigger. Transactional: a parse/check error leaves the session
    /// (and its baseline) untouched; [`SwapOutcome::NeedsRestart`] reports what only a reload
    /// can absorb and leaves the old code running.
    pub fn hot_swap(&mut self, new_source: &str) -> Result<SwapOutcome, Error> {
        let old_program = parse(&self.source)?;
        let new_program = parse(new_source)?;
        let checked = noeta_check::check_all(&new_program);
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
