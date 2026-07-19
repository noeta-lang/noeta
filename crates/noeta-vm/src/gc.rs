//! **In-run safepoint cycle collection** (memory-management 6.x): the VM half of the design in
//! `noeta-gc` — trigger polling, root enumeration, and mid-run reclamation.
//!
//! The two exit-time reapers ([`Vm::teardown`]) bound residency only at program end; a program
//! building reference cycles in a loop grew without bound until then. The safepoint path lets the
//! tier-0 dispatch loop run a collection *during* execution, at points where the full root set is
//! enumerable, under the semantic rule pinned in `noeta-gc`: **a safepoint collection never runs a
//! destructor** — destructor-bearing dead components are deferred intact to the exit collection
//! (same members, same reverse-`seq` order, same output as before), while destructor-free garbage
//! is reclaimed immediately (invisible by the destructor spec §1, so the eval↔VM differential
//! holds with no cross-backend safepoint synchronization).
//!
//! **Poll sites** (each one thread-local-bool read when idle): taken loop back-edges and frame
//! transfers in the dispatch loop, and each round of the two scheduler drive loops
//! ([`Vm::drive_future`] / `join_scope`) so an async program parked on `.await` still collects.
//! Tier-1 native code never polls — it rejoins these sites at every bail, call, and return, so a
//! JIT'd frame is never interrupted at an unsafe point. Native (extension) dispatches never poll:
//! a `NativeCtx` drive passes no safepoint view, since extension Rust frames can hold values the
//! VM cannot enumerate.
//!
//! **Trigger**: allocation-watermark in `Trace` mode, candidate-buffer growth in `TrialDeletion`
//! mode — armed per run/isolate (thread-local), step `NOETA_GC_THRESHOLD` (default 10k objects),
//! geometric re-arm so genuinely-live residency pays a vanishing collection frequency.
//!
//! **Safety layers** (on top of exact root enumeration): the collector aborts a `Trace`-mode
//! collection whose garbage set does not exactly refcount-balance (a missed root then costs
//! liveness until exit, never a use-after-free), and `TrialDeletion` needs no roots at all.

use crate::*;

impl<'m> Vm<'m> {
    /// Run a safepoint collection if one is due and this is a safe point to do it. `frames`/`regs`
    /// are the **outermost** run's live frame stack and register windows (empty slices when polled
    /// from a depth-0 drive loop, whose transient values ride [`Vm::transient_roots`] instead).
    ///
    /// Gates, in order:
    /// - a nested (re-entrant) run: the outer runs' register stacks live in Rust locals this poll
    ///   cannot enumerate, so collection waits for the outermost loop's next poll;
    /// - teardown/reclaim in progress (`gc_suspended`): the heap is mid-surgery (pinned cycle
    ///   members, partially-released structures) — exit collection handles everything anyway;
    /// - a hover evaluation, attached debugger, or debug session: paused/inspected state may hold
    ///   values outside the enumerable roots;
    /// - (Trace only) a sibling `VmSession` sharing this thread's heap registry: its live objects
    ///   are not in this VM's roots, so a sweep would reclaim them (the same rule as teardown's
    ///   last-owner sweep gate).
    pub(crate) fn maybe_safepoint_gc(&mut self, frames: &[Frame], regs: &[Value]) {
        if self.run_depth > 1
            || self.gc_suspended
            || self.pure_eval
            || self.debugger.is_some()
            || self.debug_session.is_some()
        {
            return;
        }
        match noeta_value::collector_mode() {
            noeta_value::CollectorMode::Trace => {
                if crate::lifecycle::session_heap_owner_count() > 1 {
                    return;
                }
                let roots = self.safepoint_roots(frames, regs);
                let destructors = &self.destructors;
                let garbage = noeta_gc::collect_trace_safepoint(&roots, &|v| {
                    v.shape().is_some_and(|s| destructors.contains_key(&s.name))
                });
                self.reclaim_cycle_garbage(garbage);
            }
            noeta_value::CollectorMode::TrialDeletion => {
                let destructors = &self.destructors;
                let garbage = noeta_gc::collect_trial_deletion_safepoint(&|v| {
                    v.shape().is_some_and(|s| destructors.contains_key(&s.name))
                });
                self.reclaim_cycle_garbage(garbage);
            }
        }
        noeta_value::safepoint_gc_rearm();
    }

    /// Enumerate every owned reference the VM holds at a safepoint — the mid-run root set the
    /// trace marks from. The register-window invariant makes `regs` exact: at every op boundary
    /// each active window slot holds either an owned reference or `unit` (drops clear their
    /// register, moves clear their source, returns truncate), the same invariant the abort
    /// teardown already relies on to release each window exactly once.
    fn safepoint_roots(&self, frames: &[Frame], regs: &[Value]) -> Vec<Value> {
        let mut roots: Vec<Value> = Vec::with_capacity(regs.len() + 64);
        roots.extend(regs.iter().copied().filter(|v| v.is_pointer()));
        for frame in frames {
            roots.extend(frame.upvalues.iter().copied());
        }
        roots.extend(
            self.persist
                .globals
                .iter()
                .copied()
                .filter(|v| v.is_pointer()),
        );
        // Undrained channel messages (Local buffers own one reference each; Shared queues hold
        // `Wire` copies, not heap values).
        for chan in &self.persist.channels {
            if let Channel::Local { buffer, .. } = chan {
                roots.extend(buffer.iter().map(|(msg, _)| *msg));
            }
        }
        // The extension arena and embed handles: the same first-class root sets teardown feeds
        // into its pre-teardown trace.
        roots.extend(self.persist.ext_arena.iter().flatten().copied());
        roots.extend(self.persist.embed_handles.iter().flatten().copied());
        roots.extend(self.sched.traced_futures.iter().map(|t| t.future));
        // Scheduler-held tasks: each open `concurrent` scope owns its tasks' futures (and parked
        // results) — the roots that make an async cycle-builder collectable while parked.
        for scope in &self.sched.scopes {
            for task in scope {
                roots.push(task.future);
                if let Some(result) = task.result {
                    roots.push(result);
                }
            }
        }
        // Promoted-argument sources (P-PAR S2): retained for the promote memo's lifetime.
        roots.extend(self.isolates.promote_sources.iter().copied());
        // Values a depth-0 drive loop holds in Rust locals (the worker isolate's callee/future).
        roots.extend(self.transient_roots.iter().copied());
        #[cfg(feature = "jit-rt")]
        {
            roots.extend(self.tier1.jit_cache_pins.iter().copied());
            roots.push(self.tier1.jit_ret);
        }
        roots
    }
}
