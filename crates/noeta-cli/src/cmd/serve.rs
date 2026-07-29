//! The serving/hot-reload engines: extension-contributed commands (`noeta serve`), the
//! multi-core `--parallel` worker pool, and the in-process hot-reload run paths.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use noeta_ast::{Expr, Stmt};
use noeta_diagnostics::render;
use noeta_pm::{graph, manifest};
use noeta_runner::compile::Loaded;
use noeta_runner::compile_real;
use noeta_span::{SourceMap, Span};
use noeta_vm::VmBackend;

use crate::cmd::run::{p2p_app_namespace, run_program};
use crate::output::emit_diagnostics_mapped;
use crate::{compose, watch};

/// The request-handler function `noeta serve` drives — the one name a served program must define.
const HANDLER: &str = "fetch";

/// The module the serve entry call names, qualified — see [`EntryCall::module`](noeta_stdlib::EntryCall).
/// The multi-core path never reaches `run_file` (it binds the listener itself), so it assembles the
/// same `EntryCall` from here; both paths therefore name one module and build one call.
const SERVE_ENTRY_MODULE: &str = "std.http.server";

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

        // Dependency packages so `noeta serve` (and any entry-call command) sees the same
        // cross-package `use <dep-key>.…` the plain `run` path does (package-manager P2.1c) —
        // the compose probe's graph when it resolved (audit-5 F2), else resolved here so the
        // error renders on this path.
        let (deps, package_uses) = match resolved {
            Some(graph) => (graph.packages, graph.package_uses),
            None => match graph::resolve_graph(file) {
                Ok(graph) => (graph.packages, graph.package_uses),
                Err(err) => {
                    eprintln!("noeta: {err}");
                    return 2;
                }
            },
        };
        // The entry call and the `use` that binds it, handed to the *loader* rather than pushed
        // onto the linked program: a synthesized statement has to go through the linker exactly as
        // a handwritten one does, or its import resolves nothing and its names are left unqualified
        // (see `load_with_deps_appending`).
        let tail = entry.map(entry_tail).unwrap_or_default();
        let linked = match noeta_loader::load_with_deps_appending(
            file,
            manifest::root_edition(file),
            &deps,
            &package_uses,
            &tail,
        ) {
            Err(err) => {
                eprintln!("noeta: cannot read {}: {err}", file.display());
                return 2;
            }
            Ok(Err(load_diagnostics)) => {
                let mut stderr = io::stderr();
                for ld in &load_diagnostics {
                    let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
                }
                return 1;
            }
            Ok(Ok(linked)) => linked,
        };
        // Rewrap as the runner's `Loaded` so the type check below rides `Loaded::check` — the
        // editions-threading choke point (audit-3 F8).
        let loaded = crate::context::loaded(linked);

        // Armed, not printed: the serve loop emits it once the listener is actually bound, so a
        // bind clash or a check error is never preceded by "listening on …".
        if let Some(banner) = banner {
            noeta_stdlib::serve::arm_serve_banner(banner.to_string());
        }
        // Hot mode (server-hmr W1, armed by the `--watch` wrapper for `serve`): run through the
        // debug-session machinery with the hot-swap mailbox, so edits swap into the LIVE process.
        if std::env::var_os("NOETA_HOT").is_some() {
            return run_program_hot(file, &loaded);
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
        port: i64,
        host: &str,
        workers: usize,
    ) -> u8 {
        serve_parallel_impl(file, port, host, workers)
    }
}

/// Multi-core `noeta serve --parallel N` (server-hmr S1): bind the listener ONCE, then run the
/// serve program in `workers` OS-thread isolates, each with a real host adopting a `try_clone`d
/// dup of the listening socket. Intra-process fds are shared across threads, so the kernel
/// load-balances `accept()` across the workers with no `SO_REUSEPORT`/`socket2` and no new dep.
/// The process-wide shutdown flag (S0) drains every worker on SIGINT.
pub(crate) fn serve_parallel_impl(
    file: &std::path::Path,
    port: i64,
    host: &str,
    workers: usize,
) -> u8 {
    let resolved = match compose::maybe_delegate(file) {
        Err(_) => return 1,
        Ok(resolved) => resolved,
    };
    // The compose probe's graph when it resolved (audit-5 F2), else resolved here so the error
    // renders on this path.
    let (deps, package_uses) = match resolved {
        Some(graph) => (graph.packages, graph.package_uses),
        None => match graph::resolve_graph(file) {
            Ok(graph) => (graph.packages, graph.package_uses),
            Err(err) => {
                eprintln!("noeta: {err}");
                return 2;
            }
        },
    };
    // The multi-core entry call is the same `server.serve(port, fetch, host)` the single-worker path
    // makes, built the same way and handed to the loader the same way — including the hot-mode
    // handler trampoline, which `entry_tail` applies to an `Ident` argument (server-hmr F5).
    let tail = entry_tail(&noeta_stdlib::EntryCall {
        module: SERVE_ENTRY_MODULE,
        func: "serve",
        args: vec![
            noeta_stdlib::EntryArg::Int(port),
            noeta_stdlib::EntryArg::Ident(HANDLER),
            noeta_stdlib::EntryArg::Str(host.to_string()),
        ],
    });
    let linked = match noeta_loader::load_with_deps_appending(
        file,
        manifest::root_edition(file),
        &deps,
        &package_uses,
        &tail,
    ) {
        Err(err) => {
            eprintln!("noeta: cannot read {}: {err}", file.display());
            return 2;
        }
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            return 1;
        }
        Ok(Ok(linked)) => linked,
    };
    // Rewrap as the runner's `Loaded` so the type check below rides `Loaded::check` — the
    // editions-threading choke point (audit-3 F8).
    let loaded = crate::context::loaded(linked);

    let checked = loaded.check();
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&loaded.sources, checked.diagnostics.iter());
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
        return serve_parallel_hot(
            file,
            &loaded.program,
            &checked,
            &loaded.sources,
            base,
            workers,
            args,
            app_id,
            &addr,
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
    let mut handles = Vec::with_capacity(workers);
    for worker in 0..workers {
        let listener = match base.try_clone() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("noeta: cannot clone the listener for worker {worker}: {e}");
                return 1;
            }
        };
        let module = std::sync::Arc::clone(&module);
        let args = args.clone();
        let app_id = app_id.clone();
        handles.push(std::thread::spawn(move || {
            run_worker(module, listener, args, app_id)
        }));
    }
    // Every worker drains and returns on shutdown; a non-zero worker exit propagates.
    let mut code = 0u8;
    for handle in handles {
        match handle.join() {
            Ok(worker_code) => code = code.max(worker_code),
            Err(_) => code = code.max(1),
        }
    }
    code
}

/// One `--parallel` worker (server-hmr S1): a real host seeded with the pre-bound listener plus a
/// wall-clock executor, running the compiled serve module to completion (it returns when the
/// graceful-drain flag closes the accept loop).
pub(crate) fn run_worker(
    module: std::sync::Arc<noeta_bytecode::Module>,
    listener: std::net::TcpListener,
    args: Vec<String>,
    app_id: Option<String>,
) -> u8 {
    let host: Box<dyn noeta_stdlib::Host> = match noeta_host_real::RealHost::new() {
        Ok(h) => Box::new(
            h.with_args(args.clone())
                .with_p2p_app(app_id.clone())
                .with_prebound_listener(listener),
        ),
        Err(e) => {
            eprintln!("noeta: cannot start a worker runtime: {e}");
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
        let host: Box<dyn noeta_stdlib::Host> = Box::new(
            noeta_host_real::RealHost::new()
                .expect("cannot start a nested isolate's runtime")
                .with_args(args.clone())
                .with_p2p_app(app_id_for_factory.clone()),
        );
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_host_real::RealExecutor::new().expect("cannot start a nested isolate's executor"),
        );
        (host, executor)
    });
    let (result, trace, _) = VmBackend::new()
        .run_module_with_host_and_executor_parallel(module, host, executor, factory, false);
    print!("{}", result.stdout);
    let _ = io::stdout().flush();
    // The program's stderr stream (`std.io`'s `err`/`errln`) to real stderr, after stdout flushes.
    eprint!("{}", result.stderr);
    let _ = io::stderr().flush();
    if trace.len() >= 2 {
        eprintln!("[worker] aborted");
    }
    u8::try_from(result.exit_code).unwrap_or(1)
}

/// Hot multi-core serve (server-hmr F5): each of `workers` worker isolates runs the debug-session
/// hot path against a `try_clone`d listener, all sharing ONE [`noeta_vm::HotChannel`] broadcast
/// queue that a single watcher deposits into — so an edit swaps into every worker in place. The
/// `--parallel --watch` wrapper spawned this (via `NOETA_HOT`); a change the fleet cannot absorb
/// exits with the restart sentinel, restarting the whole wrapper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve_parallel_hot(
    entry_path: &std::path::Path,
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
    sources: &SourceMap,
    base: std::net::TcpListener,
    workers: usize,
    args: Vec<String>,
    app_id: Option<String>,
    addr: &str,
) -> u8 {
    let _ = sources;
    let mailbox: noeta_vm::HotSwapMailbox = std::sync::Arc::new(noeta_vm::HotChannel::default());
    let wake = std::sync::Arc::new(noeta_host_real::Notify::new());
    watch::spawn_hot_watcher(
        entry_path.to_path_buf(),
        std::sync::Arc::clone(&mailbox),
        std::sync::Arc::clone(&wake),
    );
    eprintln!(
        "noeta serve: listening on http://{addr} across {workers} workers, hot-reloading \
         (Ctrl-C to stop)"
    );
    let mut handles = Vec::with_capacity(workers);
    for worker in 0..workers {
        let listener = match base.try_clone() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("noeta: cannot clone the listener for worker {worker}: {e}");
                return 1;
            }
        };
        let program = program.clone();
        let sites = checked.sites.clone();
        let mailbox = std::sync::Arc::clone(&mailbox);
        let wake = std::sync::Arc::clone(&wake);
        let args = args.clone();
        let app_id = app_id.clone();
        handles.push(std::thread::spawn(move || {
            run_worker_hot(program, sites, listener, mailbox, wake, args, app_id)
        }));
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

/// One hot `--parallel` worker (server-hmr F5): compiles its own session (each isolate is
/// shared-nothing, so it holds its own `SessionCompiler` for fragment installs), adopts the
/// pre-bound listener, arms the shared broadcast queue + wake, and runs the debug-session hot VM.
pub(crate) fn run_worker_hot(
    program: noeta_ast::Program,
    sites: noeta_compiler::Sites,
    listener: std::net::TcpListener,
    mailbox: noeta_vm::HotSwapMailbox,
    wake: std::sync::Arc<noeta_host_real::Notify>,
    args: Vec<String>,
    app_id: Option<String>,
) -> u8 {
    let (module, compiler) =
        match noeta_compiler::compile_with_sites_session(&program, sites, false, false) {
            Ok(pair) => pair,
            Err(u) => {
                eprintln!("noeta: cannot compile a hot worker: {}", u.reason);
                return 1;
            }
        };
    let host: Box<dyn noeta_stdlib::Host> = match noeta_host_real::RealHost::new() {
        Ok(h) => Box::new(
            h.with_args(args)
                .with_p2p_app(app_id)
                .with_prebound_listener(listener),
        ),
        Err(e) => {
            eprintln!("noeta: cannot start a hot worker runtime: {e}");
            return 1;
        }
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_host_real::RealExecutor::new() {
        Ok(mut ex) => {
            ex.set_wake(wake);
            Box::new(ex)
        }
        Err(e) => {
            eprintln!("noeta: cannot start a hot worker executor: {e}");
            return 1;
        }
    };
    let (result, trace) =
        VmBackend::new().run_module_hot(&module, compiler, host, executor, mailbox);
    print!("{}", result.stdout);
    let _ = io::stdout().flush();
    // The program's stderr stream (`std.io`'s `err`/`errln`) to real stderr, after stdout flushes.
    eprint!("{}", result.stderr);
    let _ = io::stderr().flush();
    if trace.len() >= 2 {
        eprintln!("[worker] aborted");
    }
    u8::try_from(result.exit_code).unwrap_or(1)
}

/// Run an entry-call program with **in-process hot reload** (server-hmr W1) — `noeta serve
/// --watch`'s hot mode. The program compiles through the *session* compiler (kept alive for
/// fragment installs) and runs on the real host with a [`noeta_vm::HotSwapMailbox`] armed; a
/// watcher thread ([`watch::spawn_hot_watcher`]) parses/checks/diffs each edit of the entry file
/// and deposits swappable plans (blockers or non-entry edits exit with the restart sentinel the
/// `--watch` wrapper honors). JIT stays unarmed on this path (H3 lifts).
pub(crate) fn run_program_hot(entry_path: &std::path::Path, loaded: &Loaded) -> u8 {
    // `Loaded::check`: the editions ride structurally with the program they govern.
    let checked = loaded.check();
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&loaded.sources, checked.diagnostics.iter());
        return 1;
    }
    let (module, compiler) = match noeta_compiler::compile_with_sites_session(
        &loaded.program,
        checked.sites.clone(),
        // Cooperative isolates: the hot path is the debug-session path (single OS thread).
        false,
        false,
    ) {
        Ok(pair) => pair,
        Err(u) => {
            eprintln!(
                "noeta: internal error: cannot compile for hot reload: {}",
                u.reason
            );
            return 1;
        }
    };
    let mailbox: noeta_vm::HotSwapMailbox = std::sync::Arc::new(noeta_vm::HotChannel::default());
    // The wake lets the watcher rouse a *blocked* executor the moment it deposits (server-hmr
    // L3) — otherwise an idle server (accept pending, no traffic) would only apply the swap at
    // its next request (the W1 one-request lag).
    let wake = std::sync::Arc::new(noeta_host_real::Notify::new());
    watch::spawn_hot_watcher(
        entry_path.to_path_buf(),
        std::sync::Arc::clone(&mailbox),
        std::sync::Arc::clone(&wake),
    );

    let args: Vec<String> = std::env::args().collect();
    let app_id = p2p_app_namespace(&args);
    let host: Box<dyn noeta_stdlib::Host> = match noeta_host_real::RealHost::new() {
        Ok(host) => Box::new(host.with_args(args).with_p2p_app(app_id)),
        Err(e) => {
            eprintln!("noeta: cannot start the runtime: {e}");
            return 1;
        }
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_host_real::RealExecutor::new() {
        Ok(mut executor) => {
            executor.set_wake(wake);
            Box::new(executor)
        }
        Err(e) => {
            eprintln!("noeta: cannot start the async executor: {e}");
            return 1;
        }
    };
    let (result, trace) =
        VmBackend::new().run_module_hot(&module, compiler, host, executor, mailbox);
    print!("{}", result.stdout);
    let _ = io::stdout().flush();
    // The program's stderr stream (`std.io`'s `err`/`errln`) to real stderr, after stdout flushes.
    eprint!("{}", result.stderr);
    let _ = io::stderr().flush();
    emit_diagnostics_mapped(&loaded.sources, result.diagnostics.iter());
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(&trace, &loaded.sources));
    }
    u8::try_from(result.exit_code).unwrap_or(1)
}
