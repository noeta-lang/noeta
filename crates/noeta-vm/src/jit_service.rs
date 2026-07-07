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

/// One compile response: the prototype's finalized entry point (`None` = the compile failed —
/// the mutator declines the prototype and keeps interpreting) plus its fast-convention body.
pub(crate) struct Ready {
    pub proto: usize,
    pub entry: Option<noeta_jit::CompiledFn>,
    pub fast: Option<usize>,
}

pub(crate) struct JitService {
    tx: Option<mpsc::Sender<usize>>,
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
    pub fn spawn(
        module: Arc<Module>,
        helpers: Vec<(&'static str, usize)>,
        layout: noeta_jit::FrameLayout,
        template_addr: usize,
    ) -> Option<JitService> {
        let ready = Arc::new(Mutex::new(Vec::new()));
        let stats = Arc::new(Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<usize>();
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
                    noeta_jit::Jit::new(&helper_ptrs, layout, template_addr as *const u8).ok();
                while let Ok(proto) = rx.recv() {
                    let abandoned = stop_rx.load(std::sync::atomic::Ordering::Acquire);
                    let (entry, fast) = match jit.as_mut() {
                        // A discarding shutdown abandoned the queue: drain without compiling
                        // (each request still gets its response, keeping the protocol total).
                        _ if abandoned => (None, None),
                        Some(engine) => match engine.compile(&module, proto) {
                            Ok(f) => (Some(f), engine.get_fast(proto)),
                            Err(_) => (None, None),
                        },
                        // No engine (ISA unavailable): every request still gets its response.
                        None => (None, None),
                    };
                    ready_tx.lock().expect("jit mailbox poisoned").push(Ready {
                        proto,
                        entry,
                        fast,
                    });
                }
                // Channel closed (shutdown): snapshot the compile accounting, then drop the
                // engine — and with it every code page. The VM joined-then-reads, and no native
                // code runs after teardown reached shutdown, so nothing can dangle.
                *stats_tx.lock().expect("jit stats poisoned") = Some(match jit {
                    Some(engine) => JitStats {
                        native: engine.native_count(),
                        compiled: engine.compiled_count(),
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

    /// Queue prototype `proto` for compilation. `false` if the service thread is gone (the
    /// mutator then declines the prototype rather than waiting forever).
    pub fn request(&self, proto: usize) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.send(proto).is_ok())
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
