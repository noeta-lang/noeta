//! Off-thread tier-1 compilation (P-PAR S4): a background thread owns the Cranelift [`Jit`]
//! engine for its whole life, so hot-counter promotion never pauses the mutator — S0c measured
//! synchronous compiles at 3.5–145 ms *each* (up to ~194 ms worst), dominating wall time on
//! compile-heavy programs.
//!
//! Shape: the mutator sends prototype indices down an mpsc channel and keeps interpreting at
//! tier 0; the service compiles and pushes a [`Ready`] response into a mutex mailbox; the
//! mutator drains the mailbox at its existing promotion checkpoints (`jit_enter` /
//! `jit_osr_backedge`) into its **mirror tables** (`jit_entries` / `jit_fast`) — the single
//! lookup source native call helpers read, so the engine's own tables are never touched from
//! two threads. Every request gets exactly one response (a failed compile reports
//! `entry: None`, which the mutator turns into a per-prototype decline), so the mutator's
//! pending counter always drains back to zero and the mailbox mutex is only ever locked while
//! a request is actually in flight.
//!
//! Lifetime: the engine (and every finalized code page) lives on the service thread — the
//! `Jit`'s baked raw pointers make it `!Send`, and it must not drop while native code could
//! still run. [`JitService::shutdown`] is called as the **last** step of VM teardown (after
//! destructors, which may themselves call compiled functions): it closes the channel, the
//! thread drains outstanding requests, snapshots [`JitStats`], and exits — only then do the
//! pages drop. The `force_jit` oracle path keeps the synchronous on-VM engine and never
//! constructs a service, so the jit-differential is byte-identical.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use noeta_bytecode::Module;

use crate::JitStats;

/// One compile request. A prototype may need both shapes over its life — a back-edge asks for the
/// hot loop's window, a hot call entry asks for the whole prototype — so the job says which, and
/// each gets its own response.
#[derive(Clone, Copy)]
pub(crate) enum Job {
    /// The whole-prototype body (+ its fast-convention twin): the frame-entry shape.
    Main(usize),
    /// The **region-scoped OSR body** (P-OSRW) for the loop that got hot at `header`. Falls back
    /// to the whole-prototype body when the window is the whole prototype, so a back-edge-born
    /// promotion always ends up with *something* native.
    Osr { proto: usize, header: usize },
}

impl Job {
    fn proto(self) -> usize {
        match self {
            Job::Main(p) | Job::Osr { proto: p, .. } => p,
        }
    }
}

/// One compile response: the prototype's finalized entry point (`None` = the compile failed —
/// the mutator declines the prototype and keeps interpreting) plus its fast-convention body, or —
/// for a back-edge-born request the engine could scope — the region body instead.
pub(crate) struct Ready {
    pub proto: usize,
    /// Whether this answers a [`Job::Osr`] request. The mutator gates window requests on "one in
    /// flight" rather than "once ever" (a window covers one loop), so it needs to know which gate
    /// a response releases.
    pub osr: bool,
    pub entry: Option<noeta_jit::CompiledFn>,
    pub fast: Option<usize>,
    /// The region-scoped body, when the engine produced one. Mutually exclusive with `entry`.
    pub osr_body: Option<noeta_jit::OsrBody>,
}

pub(crate) struct JitService {
    tx: Option<mpsc::Sender<Job>>,
    ready: Arc<Mutex<Vec<Ready>>>,
    stats: Arc<Mutex<Option<JitStats>>>,
    /// Abandon flag: set by a discarding shutdown so the thread drains the remaining queue
    /// *without compiling* — nothing will ever execute those entries, and a CLI process should
    /// not linger at exit paying for them.
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl JitService {
    /// Spawn the compile thread. `helpers` are the runtime-helper symbol addresses and
    /// `template_addr` the VM's frame-template address, shipped as plain `usize`s (the values are
    /// baked into generated code exactly as in the synchronous path; the template `Box` lives on
    /// the VM and outlives the service). Returns `None` if the thread cannot spawn; engine
    /// construction failure inside the thread reports every request back as failed, so the
    /// mutator degrades to pure tier 0.
    ///
    /// `cancel` is the run's cancellation flag (isolate-cancel, JIT half) or `None`. It crosses as
    /// the `Arc` rather than as an address — `Arc<AtomicBool>` is `Send`+`Sync`, and handing the
    /// engine a *strong reference* is what makes the address it bakes into every loop header sound:
    /// the flag is then owned by the same object that owns the code pages, on the same thread, and
    /// outlives them by construction (see `noeta_jit::Jit::cancel_flag`). Shipping a bare `usize`
    /// here would have worked in practice and been unsound in principle — the VM drops its own
    /// clone the moment a cancellation is honored (`Vm::observe_cancel`).
    pub fn spawn(
        module: Arc<Module>,
        helpers: Vec<(&'static str, usize)>,
        layout: noeta_jit::FrameLayout,
        template_addr: usize,
        cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Option<JitService> {
        let ready = Arc::new(Mutex::new(Vec::new()));
        let stats = Arc::new(Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Job>();
        let ready_tx = Arc::clone(&ready);
        let stats_tx = Arc::clone(&stats);
        let stop_rx = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("noeta-jit".into())
            .spawn(move || {
                let helper_ptrs: Vec<(&str, *const u8)> = helpers
                    .iter()
                    .map(|&(name, addr)| (name, addr as *const u8))
                    .collect();
                let mut jit =
                    noeta_jit::Jit::new(&helper_ptrs, layout, template_addr as *const u8, cancel)
                        .ok();
                while let Ok(job) = rx.recv() {
                    let proto = job.proto();
                    let abandoned = stop_rx.load(std::sync::atomic::Ordering::Acquire);
                    let mut osr_body = None;
                    let (entry, fast) = match jit.as_mut() {
                        // A discarding shutdown abandoned the queue: drain without compiling
                        // (each request still gets its response, keeping the protocol total).
                        _ if abandoned => (None, None),
                        // No engine (ISA unavailable): every request still gets its response.
                        None => (None, None),
                        Some(engine) => {
                            // A back-edge-born request first asks for the loop's own window; the
                            // engine declines when that window IS the whole prototype, and then
                            // the whole-prototype body is the region body.
                            if let Job::Osr { header, .. } = job {
                                osr_body = engine.compile_osr(&module, proto, header);
                            }
                            match osr_body {
                                Some(_) => (None, None),
                                None => match engine.compile(&module, proto) {
                                    Ok(f) => (Some(f), engine.get_fast(proto)),
                                    Err(_) => (None, None),
                                },
                            }
                        }
                    };
                    ready_tx.lock().expect("jit mailbox poisoned").push(Ready {
                        proto,
                        osr: matches!(job, Job::Osr { .. }),
                        entry,
                        fast,
                        osr_body,
                    });
                }
                // Channel closed (shutdown): snapshot the compile accounting, then drop the
                // engine — and with it every code page. The VM joined-then-reads, and no native
                // code runs after teardown reached shutdown, so nothing can dangle.
                *stats_tx.lock().expect("jit stats poisoned") = Some(match jit {
                    Some(engine) => JitStats {
                        native: engine.native_count(),
                        compiled: engine.compiled_count(),
                        osr_windows: engine.osr_window_count(),
                        compile_ns_total: engine.compile_ns_total(),
                        compile_ns_max: engine.compile_ns_max(),
                        breakdown: engine.compile_breakdown(),
                    },
                    None => JitStats::default(),
                });
            })
            .ok()?;
        Some(JitService {
            tx: Some(tx),
            ready,
            stats,
            stop,
            handle: Some(handle),
        })
    }

    /// Queue one compile job. `false` if the service thread is gone (the mutator then declines the
    /// prototype rather than waiting forever).
    pub fn request(&self, job: Job) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.send(job).is_ok())
    }

    /// Take every response that has landed. Cheap when empty (one uncontended lock); the caller
    /// only calls this while it has requests in flight.
    pub fn drain(&self) -> Vec<Ready> {
        std::mem::take(&mut *self.ready.lock().expect("jit mailbox poisoned"))
    }

    /// Close the request channel, wait for the compile thread to exit, and return its final
    /// compile accounting. `drain` decides the outstanding queue's fate: `false` (production
    /// teardown) **abandons** it — nothing can ever execute those entries and the process should
    /// not linger at exit; `true` (the stats entry points) compiles it so promotion counts stay
    /// deterministic for tests and benches. After this returns no compiled entry point is
    /// callable — the caller must have cleared its mirror tables and finished every
    /// interpretation step first.
    pub fn shutdown(mut self, drain: bool) -> Option<JitStats> {
        if !drain {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
        }
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.stats.lock().expect("jit stats poisoned").take()
    }
}

impl Drop for JitService {
    /// Defensive join on abnormal drops (a VM dropped without teardown): the code pages must not
    /// outlive... rather, must not *die before* any possible native call, and joining here means
    /// the thread (and its pages) are gone before the VM's memory is.
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
