//! The serving/hot-reload engines: extension-contributed commands (`noeta serve`), the
//! multi-core `--parallel` worker pool, and the in-process hot-reload run paths.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use noeta_ast::{Expr, Stmt};
use noeta_pm::manifest;
use noeta_runner::compile::Loaded;
use noeta_runner::compile_real;
use noeta_span::{SourceMap, Span};
use noeta_vm::VmBackend;

use crate::cmd::run::{p2p_app_namespace, run_program};
use crate::output::emit_diagnostics_mapped;
use crate::{compose, watch};

/// The statements a synthesized [`EntryCall`](noeta_stdlib::EntryCall) contributes to the entry
/// program: the `use` that binds the module the call names, then the call —
/// `use std.http.server` + `server.serve(port, fetch, host)`.
///
/// Both go to the **loader** ([`noeta_loader::load_with_deps_appending`]), which appends them before
/// linking, so they resolve the way the same two lines written into the file would: the import pulls
/// in the module it names (a dependency package's `.noe` submodule otherwise binds nothing at all),
/// and the call's names are qualified and α-renamed alongside the entry's own. A module named by a
/// single bare segment brings no `use` — it binds nothing and leaves resolution to the program's own
/// imports (see `EntryCall::module`).
///
/// A synthetic span (offset 0) throughout: these nodes are compiler-generated, so no diagnostic
/// should ever need to locate them in the file. The program supplies any identifier the call names
/// (`fetch`, `up`); a missing one surfaces as an ordinary check error against the name the command
/// documents.
fn entry_tail(entry: &noeta_stdlib::EntryCall) -> Vec<Stmt> {
    let sp = Span::empty_at(0);
    let ident = |name: &str| Expr::Ident {
        name: noeta_ast::Name::canonical(name),
        span: sp,
    };
    let mut stmts = Vec::new();
    let mut path: Vec<String> = entry.module.split('.').map(str::to_string).collect();
    let local = path.pop().unwrap_or_default();
    if !path.is_empty() {
        stmts.push(Stmt::Use {
            path,
            names: vec![noeta_ast::UseName {
                name: local.clone(),
                alias: None,
                span: sp,
            }],
            span: sp,
        });
    }
    let hot = std::env::var_os("NOETA_HOT").is_some();
    let args: Vec<Expr> = entry
        .args
        .iter()
        .map(|arg| match arg {
            noeta_stdlib::EntryArg::Int(value) => Expr::Int {
                value: *value,
                span: sp,
            },
            noeta_stdlib::EntryArg::Str(value) => Expr::Str {
                value: value.clone(),
                span: sp,
            },
            // In hot mode (server-hmr W1), late-bind an identifier argument through a trampoline
            // closure `fn(req) => fetch(req)`: the serve loop captures its handler argument ONCE at
            // startup, but the trampoline's body re-resolves the global per call — so a hot swap
            // rebinding `fetch` reaches the live loop.
            noeta_stdlib::EntryArg::Ident(name) if hot => Expr::Closure {
                params: vec![noeta_ast::Param {
                    attrs: Vec::new(),
                    name: "req".to_string(),
                    name_span: sp,
                    ty: None,
                    default: None,
                    span: sp,
                    positional: false,
                }],
                ret: None,
                body: noeta_ast::ClosureBody::Expr(Box::new(Expr::Call {
                    callee: Box::new(ident(name)),
                    args: vec![noeta_ast::CallArg::positional(ident("req"))],
                    span: sp,
                })),
                span: sp,
            },
            noeta_stdlib::EntryArg::Ident(name) => ident(name),
        })
        .collect();
    let call = Expr::Call {
        callee: Box::new(Expr::Member {
            receiver: Box::new(ident(&local)),
            name: entry.func.to_string(),
            name_span: sp,
            span: sp,
        }),
        args: args
            .into_iter()
            .map(noeta_ast::CallArg::positional)
            .collect(),
        span: sp,
    };
    stmts.push(Stmt::Expr {
        expr: call,
        span: sp,
    });
    stmts
}

/// Build the clap subcommand for an extension command, registered under `name` — the local name a
/// `[trust.commands]` binding chose (equal to `ext.name` for std's own commands, which register
/// under their exported names).
pub(crate) fn ext_command_clap(
    name: &'static str,
    ext: &'static noeta_stdlib::ExtCommand,
) -> clap::Command {
    let mut cmd = clap::Command::new(name).about(ext.about);
    for spec in ext.args {
        cmd = cmd.arg(match spec.kind {
            noeta_stdlib::ArgKind::Path => clap::Arg::new(spec.name)
                .help(spec.help)
                .required(true)
                .value_parser(clap::value_parser!(PathBuf)),
            // The default is applied at dispatch (clap's builder `default_value` wants an
            // owned-string feature we don't enable); the spec's help text names it.
            noeta_stdlib::ArgKind::Int { .. } => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .value_parser(clap::value_parser!(i64)),
            noeta_stdlib::ArgKind::Str { .. } => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .value_parser(clap::value_parser!(String)),
            noeta_stdlib::ArgKind::Bool => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .action(clap::ArgAction::SetTrue),
            noeta_stdlib::ArgKind::OptStr => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .value_parser(clap::value_parser!(String)),
            noeta_stdlib::ArgKind::OptPath => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .value_parser(clap::value_parser!(PathBuf)),
            // An optional positional word: no `.long`, not required — clap fills declared
            // positionals left-to-right, matching `ArgKind::Word`'s declaration-order contract.
            noeta_stdlib::ArgKind::Word => clap::Arg::new(spec.name)
                .help(spec.help)
                .value_parser(clap::value_parser!(String)),
            noeta_stdlib::ArgKind::OptInt => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .value_parser(clap::value_parser!(i64)),
            // `allow_negative_numbers`: a threshold may legitimately be negative, and without it
            // clap reads the leading `-` as an unknown flag.
            noeta_stdlib::ArgKind::OptFloat => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .allow_negative_numbers(true)
                .value_parser(clap::value_parser!(f64)),
            // Repeatable: `Append` keeps every occurrence, which is what `ParsedArgs::strs` hands
            // back in order.
            noeta_stdlib::ArgKind::Strings => clap::Arg::new(spec.name)
                .long(spec.name)
                .help(spec.help)
                .action(clap::ArgAction::Append)
                .value_parser(clap::value_parser!(String)),
            // Like `Word`, an optional positional — the default is applied at dispatch, for the
            // same owned-string reason `Int`'s is.
            noeta_stdlib::ArgKind::PathDefault { .. } => clap::Arg::new(spec.name)
                .help(spec.help)
                .value_parser(clap::value_parser!(PathBuf)),
        });
    }
    cmd
}

/// Install the SIGINT graceful-drain handler for a serving process (server-hmr S0): the first
/// Ctrl-C sets the process shutdown flag and wakes any blocked serve loop (which then finishes
/// in-flight requests and returns); a second Ctrl-C forces an immediate exit. Runs on its own
/// thread with a tiny tokio runtime so it never contends with the isolate executors.
pub(crate) fn install_shutdown_handler() {
    std::thread::spawn(|| {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            // No signal runtime → the process just dies on Ctrl-C, as before. Not fatal.
            Err(_) => return,
        };
        rt.block_on(async {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            eprintln!(
                "\nnoeta serve: draining — finishing in-flight requests (Ctrl-C again to force)"
            );
            noeta_stdlib::serve::request_shutdown();
            // Wake every blocked worker (server-hmr S1: `notify_waiters` rouses all currently
            // parked accepts at once) plus one stored permit for a worker racing into its wait.
            let wake = noeta_host_real::shutdown_notify();
            wake.notify_waiters();
            wake.notify_one();
            // A second Ctrl-C during the drain forces an immediate exit.
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        });
    });
}

/// Dispatch a matched extension command: collect its declared args from the clap matches and run
/// it against the CLI's [`noeta_stdlib::CommandCtx`] driver.
pub(crate) fn ext_command_dispatch(
    ext: &'static noeta_stdlib::ExtCommand,
    matches: &clap::ArgMatches,
) -> ExitCode {
    // Graceful drain (server-hmr S0): a long-lived server honors SIGINT by finishing in-flight
    // requests and closing the listener rather than dying mid-response. `noeta serve --watch`
    // (the hot path) is Ctrl-C'd through its wrapper process instead, so only the plain path
    // installs this.
    if ext.name == "serve" && std::env::var_os("NOETA_HOT").is_none() {
        install_shutdown_handler();
    }
    let mut parsed = noeta_stdlib::ParsedArgs::default();
    for spec in ext.args {
        match spec.kind {
            noeta_stdlib::ArgKind::Path => parsed.push_path(
                spec.name,
                matches
                    .get_one::<PathBuf>(spec.name)
                    .expect("a required path argument is always present")
                    .clone(),
            ),
            noeta_stdlib::ArgKind::Int { default } => parsed.push_int(
                spec.name,
                matches
                    .get_one::<i64>(spec.name)
                    .copied()
                    .unwrap_or(default),
            ),
            noeta_stdlib::ArgKind::Str { default } => parsed.push_str(
                spec.name,
                matches
                    .get_one::<String>(spec.name)
                    .cloned()
                    .unwrap_or_else(|| default.to_string()),
            ),
            noeta_stdlib::ArgKind::Bool => parsed.push_bool(spec.name, matches.get_flag(spec.name)),
            // The default-less kinds record nothing when absent — the command's `get_*` probe
            // returns `None` and its own fallback chain (env, manifest, …) takes over.
            noeta_stdlib::ArgKind::OptStr | noeta_stdlib::ArgKind::Word => {
                if let Some(value) = matches.get_one::<String>(spec.name) {
                    parsed.push_str(spec.name, value.clone());
                }
            }
            noeta_stdlib::ArgKind::OptPath => {
                if let Some(value) = matches.get_one::<PathBuf>(spec.name) {
                    parsed.push_path(spec.name, value.clone());
                }
            }
            noeta_stdlib::ArgKind::OptInt => {
                if let Some(value) = matches.get_one::<i64>(spec.name) {
                    parsed.push_int(spec.name, *value);
                }
            }
            noeta_stdlib::ArgKind::OptFloat => {
                if let Some(value) = matches.get_one::<f64>(spec.name) {
                    parsed.push_float(spec.name, *value);
                }
            }
            // Always recorded, empty when the flag never appeared: a repeatable filter asks "which
            // names were named", and no names is an answer rather than a missing one.
            noeta_stdlib::ArgKind::Strings => parsed.push_strs(
                spec.name,
                matches
                    .get_many::<String>(spec.name)
                    .map(|vs| vs.cloned().collect())
                    .unwrap_or_default(),
            ),
            noeta_stdlib::ArgKind::PathDefault { default } => parsed.push_path(
                spec.name,
                matches
                    .get_one::<PathBuf>(spec.name)
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from(default)),
            ),
        }
    }
    ExitCode::from((ext.run)(&mut CliCommandCtx, &parsed))
}

/// The CLI's [`noeta_stdlib::CommandCtx`] driver (higher-order-abi H6): load + check a program
/// file and run it on the real host, optionally appending a synthesized trailing entry call —
/// exactly what the hardcoded `cmd_serve` did, generalized over [`noeta_stdlib::EntryCall`].
/// Layering the entry on the loaded program means the mechanism is the exact same registered
/// function a program can call directly; the command only supplies the entry convention.
pub(crate) struct CliCommandCtx;

impl noeta_stdlib::CommandCtx for CliCommandCtx {
    /// A string value from the nearest `noeta.toml`'s `[<table>] <key>` (para-extraction) — how an
    /// extension command reads its convention keys (`[db] url/migrations/seeds` for
    /// `noeta migrate`) without depending on `noeta-pm`. A raw toml lookup, deliberately lenient:
    /// a missing/unparsable manifest or a non-string value is `None` (the command's flag/env
    /// layers still apply); a genuinely malformed manifest surfaces through the verbs that parse
    /// it strictly.
    fn manifest_str(&self, table: &str, key: &str) -> Option<String> {
        let cwd = std::env::current_dir().ok()?;
        let path = manifest::find(&cwd)?;
        let text = std::fs::read_to_string(&path).ok()?;
        let parsed: toml::Table = text.parse().ok()?;
        parsed
            .get(table)?
            .as_table()?
            .get(key)?
            .as_str()
            .map(str::to_string)
    }

    fn run_file(
        &mut self,
        file: &std::path::Path,
        entry: Option<&noeta_stdlib::EntryCall>,
        banner: Option<&str>,
    ) -> u8 {
        // An extension command drives a program run; a native-dep app delegates to its composed
        // toolchain like every other verb (composition failure is fatal, never a stock fallback).
        let resolved = match compose::maybe_delegate(file) {
            Err(_) => return 1,
            Ok(resolved) => resolved,
        };
        // The entry call and the `use` that binds it, handed to the *loader* rather than pushed
        // onto the linked program: a synthesized statement has to go through the linker exactly as
        // a handwritten one does, or its import resolves nothing and its names are left unqualified
        // (see `load_with_deps_appending`).
        let tail = entry.map(entry_tail).unwrap_or_default();
        // The one front half (audit-10): dependency graph → loader-with-tail → the runner's
        // `Loaded`, shared with the `--parallel` path and with the hot watcher's re-link.
        let (loaded, entry_source, front) = match crate::context::load_entry_with_tail(
            file,
            &tail,
            crate::context::Front::Resolve(resolved.map(Box::new)),
        ) {
            Ok(program) => program.into_loaded(),
            Err(failure) => return crate::context::report_u8(&failure),
        };

        // Armed, not printed: the serve loop emits it once the listener is actually bound, so a
        // bind clash or a check error is never preceded by "listening on …".
        if let Some(banner) = banner {
            noeta_stdlib::serve::arm_serve_banner(banner.to_string());
        }
        // Hot mode (server-hmr W1, armed by the `--watch` wrapper for `serve`): run through the
        // debug-session machinery with the hot-swap mailbox, so edits swap into the LIVE process.
        if std::env::var_os("NOETA_HOT").is_some() {
            let baseline = watch::EntryUnit::of(&loaded.program, &entry_source);
            return run_program_hot(file, &loaded, tail, baseline, front);
        }
        // An entry call injected before compiling means the module differs from `run`'s for the
        // same source — a command run must never share the startup cache's `(source+tiers)` key,
        // and stays on the uncached `run_program` path (commands like `serve` are also
        // long-lived, so they would barely benefit). Commands take no program pass-through args;
        // the program sees the real process argv.
        u8::try_from(run_program(&loaded, std::env::args().collect())).unwrap_or(1)
    }

    fn serve_parallel(
        &mut self,
        file: &std::path::Path,
        entry: &noeta_stdlib::EntryCall,
        host: &str,
        port: i64,
        workers: usize,
    ) -> u8 {
        serve_parallel_impl(file, entry, host, port, workers)
    }
}

/// Multi-core `noeta serve --parallel N` (server-hmr S1): bind the listener ONCE, then run the
/// serve program in `workers` OS-thread isolates, each with a real host adopting a `try_clone`d
/// dup of the listening socket. Intra-process fds are shared across threads, so the kernel
/// load-balances `accept()` across the workers with no `SO_REUSEPORT`/`socket2` and no new dep.
/// The process-wide shutdown flag (S0) drains every worker on SIGINT.
///
/// `entry` is the command's own [`noeta_stdlib::EntryCall`] — the *same value* the single-worker
/// path runs, handed down rather than rebuilt here (audit-10). The CLI used to hand-write a twin of
/// it from local `SERVE_ENTRY_MODULE`/`HANDLER` constants, under a comment asserting the two were
/// "built the same way".
pub(crate) fn serve_parallel_impl(
    file: &std::path::Path,
    entry: &noeta_stdlib::EntryCall,
    host: &str,
    port: i64,
    workers: usize,
) -> u8 {
    let resolved = match compose::maybe_delegate(file) {
        Err(_) => return 1,
        Ok(resolved) => resolved,
    };
    // The same tail the single-worker path builds from the same declaration — including the
    // hot-mode handler trampoline, which `entry_tail` applies to an `Ident` argument (F5).
    let tail = entry_tail(entry);
    // The one front half (audit-10), shared with the single-worker path and the hot re-link.
    let (loaded, entry_source, front) = match crate::context::load_entry_with_tail(
        file,
        &tail,
        crate::context::Front::Resolve(resolved.map(Box::new)),
    ) {
        Ok(program) => program.into_loaded(),
        Err(failure) => return crate::context::report_u8(&failure),
    };

    let checked = loaded.check();
    // Report first, gate on errors only — a warning must not stop a server from starting.
    emit_diagnostics_mapped(
        &loaded.sources,
        loaded.warnings.iter().chain(checked.diagnostics.iter()),
    );
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
        return 1;
    }

    // Bind the listening socket once; each worker inherits a cloned fd.
    let addr = format!("{host}:{port}");
    let base = match std::net::TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("noeta: cannot bind `{addr}`: {e}");
            return 1;
        }
    };
    let args: Vec<String> = std::env::args().collect();
    let app_id = p2p_app_namespace(&args);

    // Hot multi-core (F5): each worker runs the debug-session hot path; ONE watcher deposits into
    // the shared broadcast queue and every worker drains it, so a swap spans the whole fleet.
    if std::env::var_os("NOETA_HOT").is_some() {
        let baseline = watch::EntryUnit::of(&loaded.program, &entry_source);
        eprintln!(
            "noeta serve: listening on http://{addr} across {workers} workers, hot-reloading \
             (Ctrl-C to stop)"
        );
        // One consumer per worker isolate: the queue reclaims a deposited plan (its fragment AST
        // and its whole-program `Sites` bundle) only once ALL of them have installed it, so a
        // worker still compiling its session — or parked mid-request — cannot lose a swap.
        let rig = HotRig::arm(file, tail, baseline, front, workers);
        let program = std::sync::Arc::new(loaded.program);
        let sites = std::sync::Arc::new(checked.sites);
        let sources = std::sync::Arc::new(loaded.sources);
        return spawn_fleet(
            &base,
            workers,
            std::sync::Arc::new(move |_worker, listener| {
                rig.run_isolate(
                    &program,
                    sites.as_ref().clone(),
                    &sources,
                    HotIsolate {
                        listener: Some(listener),
                        args: args.clone(),
                        app_id: app_id.clone(),
                        label: Some(WORKER_ABORT),
                    },
                )
            }),
        );
    }

    let module = match compile_real(&loaded.program, &checked) {
        Ok(m) => std::sync::Arc::new(m),
        Err(err) => {
            eprintln!("noeta: {err}");
            return 1;
        }
    };
    eprintln!("noeta serve: listening on http://{addr} across {workers} workers (Ctrl-C to stop)");
    let sources = loaded.sources;
    spawn_fleet(
        &base,
        workers,
        std::sync::Arc::new(move |_worker, listener| {
            run_worker(
                std::sync::Arc::clone(&module),
                listener,
                args.clone(),
                app_id.clone(),
                sources.clone(),
            )
        }),
    )
}

/// Run `body` in `workers` OS threads, each handed its own `try_clone`d dup of the bound listener,
/// and join them — the fleet's shape, shared by the cold and hot `--parallel` paths.
///
/// Every worker drains and returns on shutdown; a non-zero worker exit propagates as the fleet's.
/// A listener that cannot be duplicated is fatal for the whole fleet: a serve process that quietly
/// came up with fewer workers than asked for would look identical to one that came up with all of
/// them, only slower.
fn spawn_fleet(
    base: &std::net::TcpListener,
    workers: usize,
    body: std::sync::Arc<dyn Fn(usize, std::net::TcpListener) -> u8 + Send + Sync>,
) -> u8 {
    let mut handles = Vec::with_capacity(workers);
    for worker in 0..workers {
        let listener = match base.try_clone() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("noeta: cannot clone the listener for worker {worker}: {e}");
                return 1;
            }
        };
        let body = std::sync::Arc::clone(&body);
        handles.push(std::thread::spawn(move || body(worker, listener)));
    }
    let mut code = 0u8;
    for handle in handles {
        match handle.join() {
            Ok(worker_code) => code = code.max(worker_code),
            Err(_) => code = code.max(1),
        }
    }
    code
}

/// The real host **every serving isolate** runs on: the process argv, the p2p app namespace, live
/// output, and — for a `--parallel` worker — the pre-bound listener it adopts.
///
/// Live output is not optional here, and this is why it is one function: a server by construction
/// never exits on its own, so without it every byte the program prints is buffered for the whole
/// process lifetime and an `echo` from a request handler appears *never*. Four hand-written copies
/// of this chain existed (cold worker, hot worker, single hot worker, and the nested isolate
/// factory), and two of them were missing the flag until audit row 1 found them.
fn serve_host(
    args: Vec<String>,
    app_id: Option<String>,
    listener: Option<std::net::TcpListener>,
) -> Result<Box<dyn noeta_stdlib::Host>, String> {
    let host = noeta_host_real::RealHost::new()
        .map_err(|e| format!("cannot start the serve runtime: {e}"))?
        .with_args(args)
        .with_p2p_app(app_id)
        .with_live_output(true);
    Ok(match listener {
        Some(listener) => Box::new(host.with_prebound_listener(listener)),
        None => Box::new(host),
    })
}

/// One `--parallel` worker (server-hmr S1): a real host seeded with the pre-bound listener plus a
/// wall-clock executor, running the compiled serve module to completion (it returns when the
/// graceful-drain flag closes the accept loop).
///
/// `sources` is the fleet's source map, cloned per worker so the worker's own tail can render an
/// abort properly: this path used to print the five words `[worker] aborted` and no stack, in a
/// project that ships production stack traces on both backends (audit row 1).
pub(crate) fn run_worker(
    module: std::sync::Arc<noeta_bytecode::Module>,
    listener: std::net::TcpListener,
    args: Vec<String>,
    app_id: Option<String>,
    sources: SourceMap,
) -> u8 {
    let host = match serve_host(args.clone(), app_id.clone(), Some(listener)) {
        Ok(host) => host,
        Err(msg) => {
            eprintln!("noeta: {msg}");
            return 1;
        }
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_host_real::RealExecutor::new() {
        Ok(ex) => Box::new(ex),
        Err(e) => {
            eprintln!("noeta: cannot start a worker executor: {e}");
            return 1;
        }
    };
    let app_id_for_factory = app_id.clone();
    let factory: noeta_vm::IsolateFactory = std::sync::Arc::new(move || {
        // A nested isolate binds no listener of its own — it inherits the worker's accept loop.
        let host = serve_host(args.clone(), app_id_for_factory.clone(), None)
            .expect("cannot start a nested isolate's runtime");
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_host_real::RealExecutor::new().expect("cannot start a nested isolate's executor"),
        );
        (host, executor)
    });
    let (result, trace, _) = VmBackend::new()
        .run_module_with_host_and_executor_parallel(module, host, executor, factory, false, None);
    emit_run_tail(&result, &trace, &sources, Some(WORKER_ABORT))
}

/// The banner a `--parallel` worker labels an abort with, before everything the runtime has to say
/// about it. The single-worker path passes `None` — it *is* the process, so there is no "which one"
/// to answer.
const WORKER_ABORT: &str = "[worker] aborted";

// ---------------------------------------------------------------------- the one hot install

/// The **process-wide half of a hot install** (audit-10, steps 4–6 of the hot sequence): the
/// hot-swap mailbox sized for its consumers, the wake that rouses a blocked executor, and the
/// watcher thread armed over both.
///
/// It exists because `noeta serve` assembled this sequence twice — once for the single worker and
/// once for the fleet — and the only genuine difference between the two was the consumer count.
/// Every other difference between them was drift: the parallel path took a `&SourceMap` it never
/// used (its first statement was `let _ = sources;`), because the step that consumes one was never
/// copied across. A parameterised install has no delta to drift in.
///
/// **The watcher is armed before anything compiles**, deliberately: a save made in the seconds a
/// cold compile takes must already be queued when the run thread starts. The single-worker path
/// used to compile first and arm second, so that save was silently lost; the fleet armed first.
/// One order now, and it is the safe one.
#[derive(Clone)]
pub(crate) struct HotRig {
    mailbox: noeta_vm::HotSwapMailbox,
    wake: std::sync::Arc<noeta_host_real::Notify>,
}

/// The per-isolate inputs of a hot install: what this one isolate has that its siblings do not.
pub(crate) struct HotIsolate {
    /// The pre-bound listener this isolate adopts (`--parallel`), or `None` when the program binds
    /// its own socket (the single worker).
    listener: Option<std::net::TcpListener>,
    args: Vec<String>,
    app_id: Option<String>,
    /// How an abort is labelled — [`WORKER_ABORT`] in the fleet, `None` for the single worker.
    label: Option<&'static str>,
}

impl HotRig {
    /// Steps 4–6: build the mailbox for `consumers` isolates, build the wake, arm the watcher.
    ///
    /// `consumers` is the count the queue gates reclamation on, so it must equal the number of VMs
    /// that will arm this mailbox — 1 for `serve --watch`, N for `serve --parallel N --watch`. It
    /// is the *only* parameter, because it is the only real difference between the two installs.
    ///
    /// The wake lets the watcher rouse a *blocked* executor the moment it deposits (server-hmr L3)
    /// — otherwise an idle server (accept pending, no traffic) would only apply the swap at its
    /// next request (the W1 one-request lag).
    fn arm(
        entry_path: &std::path::Path,
        tail: Vec<Stmt>,
        baseline: watch::EntryUnit,
        front: std::sync::Arc<noeta_runner::compile::FrontFacts>,
        consumers: usize,
    ) -> HotRig {
        let mailbox: noeta_vm::HotSwapMailbox =
            std::sync::Arc::new(noeta_vm::HotChannel::new(consumers));
        let wake = std::sync::Arc::new(noeta_host_real::Notify::new());
        watch::spawn_hot_watcher(
            entry_path.to_path_buf(),
            tail,
            baseline,
            front,
            std::sync::Arc::clone(&mailbox),
            std::sync::Arc::clone(&wake),
        );
        HotRig { mailbox, wake }
    }

    /// Steps 3 and 7–9 for one isolate: compile its own session (each isolate is shared-nothing, so
    /// it holds its own `SessionCompiler` for fragment installs), build the real host, build the
    /// executor **and give it this rig's wake**, run the hot VM, render the tail.
    ///
    /// Every hot isolate in the process runs this — the fleet's workers and the single worker
    /// alike — so the two cannot serve a swap differently. That equality is exactly what two
    /// implementations were free to break, and it is what `parallel_hot`/`hot_serve` pin.
    fn run_isolate(
        &self,
        program: &noeta_ast::Program,
        sites: noeta_compiler::Sites,
        sources: &SourceMap,
        isolate: HotIsolate,
    ) -> u8 {
        let (module, compiler) =
            match noeta_compiler::compile_with_sites_session(program, sites, false, false) {
                Ok(pair) => pair,
                Err(u) => {
                    eprintln!("noeta: cannot compile for hot reload: {}", u.reason);
                    return 1;
                }
            };
        let host = match serve_host(isolate.args, isolate.app_id, isolate.listener) {
            Ok(host) => host,
            Err(msg) => {
                eprintln!("noeta: {msg}");
                return 1;
            }
        };
        let executor: Box<dyn noeta_stdlib::Executor> = match noeta_host_real::RealExecutor::new() {
            Ok(mut executor) => {
                executor.set_wake(std::sync::Arc::clone(&self.wake));
                Box::new(executor)
            }
            Err(e) => {
                eprintln!("noeta: cannot start the async executor: {e}");
                return 1;
            }
        };
        let (result, trace) = VmBackend::new().run_module_hot(
            &module,
            compiler,
            host,
            executor,
            std::sync::Arc::clone(&self.mailbox),
        );
        emit_run_tail(&result, &trace, sources, isolate.label)
    }
}

/// The shared run epilogue of every serving isolate (audit row 1): the program's stdout, its own
/// stderr stream, the run's diagnostics and the abort traceback — which on the worker paths used to
/// be the five words `[worker] aborted` and no stack.
///
/// This writes the tail's [`parts`](noeta_backend::RunTail::parts) rather than calling
/// `emit_status`, because a worker labels its failure: the `[worker] aborted` banner belongs after
/// the program's own stdout and before everything the runtime has to say about the abort. Reading
/// the parts is what makes that possible without re-deriving the epilogue — and a component added
/// to a run lands in the right half of this split with no edit here. `label` of `None` is the plain
/// `RunTail` order with nothing inserted.
fn emit_run_tail(
    result: &noeta_backend::RunResult,
    trace: &[noeta_vm::TraceFrame],
    sources: &SourceMap,
    label: Option<&str>,
) -> u8 {
    let tail = noeta_backend::RunTail::render(result, trace, sources);
    let (mut out, mut err) = (io::stdout(), io::stderr());
    for part in tail.parts_for(noeta_backend::Stream::Stdout) {
        let _ = out.write_all(part.text.as_bytes());
    }
    let _ = out.flush();
    if let Some(label) = label
        && tail.aborted()
    {
        let _ = writeln!(err, "{label}");
    }
    for part in tail.parts_for(noeta_backend::Stream::Stderr) {
        let _ = err.write_all(part.text.as_bytes());
    }
    let _ = err.flush();
    tail.status()
}

/// Run an entry-call program with **in-process hot reload** (server-hmr W1) — `noeta serve
/// --watch`'s hot mode. The program compiles through the *session* compiler (kept alive for
/// fragment installs) and runs on the real host with a [`noeta_vm::HotSwapMailbox`] armed; a
/// watcher thread ([`watch::spawn_hot_watcher`]) re-links/checks/diffs each edit of the entry file
/// and deposits swappable plans (blockers or non-entry edits exit with the restart sentinel the
/// `--watch` wrapper honors). JIT stays unarmed on this path (H3 lifts).
///
/// A fleet of exactly one: the install is [`HotRig`], the same nine steps the `--parallel` fleet
/// runs, with one consumer instead of N.
pub(crate) fn run_program_hot(
    entry_path: &std::path::Path,
    loaded: &Loaded,
    tail: Vec<Stmt>,
    baseline: watch::EntryUnit,
    front: std::sync::Arc<noeta_runner::compile::FrontFacts>,
) -> u8 {
    // `Loaded::check`: the editions ride structurally with the program they govern.
    let checked = loaded.check();
    // Report first, gate on errors only — the same rule `noeta run` follows.
    emit_diagnostics_mapped(
        &loaded.sources,
        loaded.warnings.iter().chain(checked.diagnostics.iter()),
    );
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
        return 1;
    }
    let rig = HotRig::arm(entry_path, tail, baseline, front, 1);
    let args: Vec<String> = std::env::args().collect();
    let app_id = p2p_app_namespace(&args);
    rig.run_isolate(
        &loaded.program,
        checked.sites,
        &loaded.sources,
        HotIsolate {
            // The single worker's program binds its own socket through `server.serve`.
            listener: None,
            args,
            app_id,
            label: None,
        },
    )
}

#[cfg(test)]
mod tests {
    //! The install is written **once** (audit-10).
    //!
    //! Nine steps, two implementations, and the only real difference between them was one consumer
    //! versus N — that was the shape of this row, and folding the two into [`HotRig`] is only half a
    //! fix. Nothing stops the next feature from growing a third copy beside it, and the previous two
    //! looked perfectly reasonable at every commit that made them diverge. So each step of the hot
    //! install is censused here by the one call that performs it: a second `HotChannel::new`, a
    //! second `set_wake`, a second `RealHost::new` in these two files means a second install, and
    //! the census names the step rather than leaving it to be found by a swap that quietly stopped
    //! reaching one worker.
    //!
    //! Comments and doc comments are stripped before counting, and the census stops at this module.
    //! `tests/cli/automation.rs` records why: a gate a *comment* can satisfy reads as coverage and
    //! is worse than no gate. Every token below therefore matches real code or nothing.

    /// The hot-install source: `cmd/serve.rs` and `watch.rs`, comment-free, up to this module.
    fn install_source() -> String {
        let serve = include_str!("serve.rs");
        let watch = include_str!("../watch.rs");
        let mut text = String::new();
        for file in [serve, watch] {
            for line in file.lines() {
                let trimmed = line.trim_start();
                // Everything from the test module down is census scaffolding, not the install.
                if trimmed.starts_with("#[cfg(test)]") {
                    break;
                }
                if trimmed.starts_with("//") {
                    continue;
                }
                text.push_str(line);
                text.push('\n');
            }
        }
        text
    }

    /// Each step of the hot install, and the single call that performs it.
    ///
    /// `spawn_hot_watcher` is 2: its definition in `watch.rs` and the one call from `HotRig::arm`.
    /// Everything else is exactly one occurrence in the pair of files.
    const INSTALL_STEPS: &[(&str, usize, &str)] = &[
        (
            "compile_with_sites_session(",
            1,
            "step 3: each isolate compiles its own session",
        ),
        (
            "HotChannel::new(",
            1,
            "step 4: the mailbox, sized for its consumer count — the ONE genuine difference \
             between the single worker and the fleet, and therefore a parameter, not a copy",
        ),
        (
            "Notify::new(",
            1,
            "step 5: the wake that rouses a blocked executor when the watcher deposits",
        ),
        (
            "spawn_hot_watcher(",
            2,
            "step 6: the watcher — its definition, and the one call from HotRig::arm",
        ),
        (
            "RealHost::new(",
            1,
            "step 7: the real host, which is where `with_live_output` was twice forgotten",
        ),
        (
            "set_wake(",
            1,
            "step 8: the executor is given the rig's wake",
        ),
        ("run_module_hot(", 1, "step 9a: the hot VM run itself"),
        (
            "RunTail::render(",
            1,
            "step 9b: the run tail (audit row 1's chokepoint)",
        ),
    ];

    #[test]
    fn every_step_of_the_hot_install_is_performed_in_exactly_one_place() {
        let source = install_source();
        for (token, want, step) in INSTALL_STEPS {
            let found = source.matches(token).count();
            assert_eq!(
                found, *want,
                "`{token}` occurs {found} time(s) in the serve/watch pair, expected {want} — \
                 {step}.\nA second occurrence is a second hot install: route the new caller \
                 through `HotRig` instead. A change that legitimately moves this step must update \
                 the census."
            );
        }
    }

    /// The front half is shared too, and the way it used to drift was that each site called the
    /// loader and the resolver *itself*. Both now go through `context::load_entry_with_tail`, so
    /// neither name appears in these files at all.
    #[test]
    fn the_serve_front_half_resolves_and_links_through_the_shared_seam() {
        let source = install_source();
        for token in [
            "load_with_deps_appending(",
            "load_with_deps(",
            "resolve_graph(",
        ] {
            assert_eq!(
                source.matches(token).count(),
                0,
                "`{token}` is called directly in the serve/watch pair — the front half is \
                 `context::load_entry_with_tail`, which resolves through the runner's `FrontFacts` \
                 firewall and lets the hot re-link reuse the boot's graph (audit-10)"
            );
        }
    }
}
