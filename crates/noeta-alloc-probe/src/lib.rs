//! A test-only heap-residency probe.
//!
//! [`TrackingAlloc`] is a pass-through [`GlobalAlloc`] wrapping the system allocator that maintains
//! two process-wide counters: the currently-live byte total and its all-time high-water mark.
//! Register it as the test binary's `#[global_allocator]` and bracket a region with [`peak_during`]
//! to learn the peak heap a piece of work touched. This is how the P-PACK memory-density wins are
//! measured (flat `Vec<u64>` buffers vs N boxed objects) — a number the time benchmarks cannot see.
//!
//! It is deliberately a standalone crate so that the `unsafe` it needs to implement
//! [`GlobalAlloc`] stays out of the runtime crates, and so the workspace `unsafe_code = "forbid"`
//! quarantine is relaxed in exactly one extra place. Tests use it as a dev-dependency; the `noeta`
//! **binary** also registers it, so the allocation profiler (`noeta profile --alloc`) can read
//! [`thread_allocated`] — a per-thread *cumulative* allocated-bytes counter — and attribute each
//! per-op delta to the interpreter's live call stack. The whole cost is two relaxed atomic ops and
//! one thread-local add per allocation, pass-through otherwise.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

thread_local! {
    /// Cumulative bytes this thread has allocated (monotonic; frees do not subtract). Const-
    /// initialized so reading it inside the allocator never itself allocates.
    static THREAD_ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

/// Cumulative bytes allocated **by the calling thread** since it started (monotonic — frees are
/// not subtracted, so a delta between two reads is "bytes allocated in between"). Thread-local, so
/// an interpreter thread's reads are undisturbed by host/runtime threads. Always 0 unless the
/// process registered [`TrackingAlloc`] as its `#[global_allocator]` (the `noeta` binary does).
pub fn thread_allocated() -> u64 {
    THREAD_ALLOCATED.with(|c| c.get())
}

/// Currently-live bytes: the sum of outstanding allocation sizes right now (each `alloc` adds its
/// layout size, each `dealloc` subtracts it). Unlike [`peak_during`]'s transient high-water mark,
/// this is the **residency** — what a heap-retention regression test brackets across repeated
/// work to prove a data structure is not accumulating (audit F9: salsa deleted-input reclamation).
/// Always 0 unless the process registered [`TrackingAlloc`] as its `#[global_allocator]`.
pub fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// Currently-live bytes (sum of outstanding allocation sizes).
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// High-water mark of [`LIVE`] since the last [`peak_during`] reset.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// A `#[global_allocator]`-ready counting allocator wrapping any inner [`GlobalAlloc`] (the system
/// allocator by default — the test probe; the `noeta` binary wraps its mimalloc). The default
/// [`GlobalAlloc::realloc`] is composed from `alloc`/`dealloc`, so resizes are counted.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackingAlloc<A: GlobalAlloc = System>(pub A);

// SAFETY: every call forwards directly to the inner allocator with the same layout; the atomic and
// thread-local bookkeeping only reads/writes counters and never touches the returned memory.
unsafe impl<A: GlobalAlloc> GlobalAlloc for TrackingAlloc<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.0.alloc(layout) };
        if !ptr.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
            // `try_with`: during thread teardown the TLS slot may already be destroyed while the
            // allocator is still called — skip the bump rather than abort.
            let _ = THREAD_ALLOCATED.try_with(|c| c.set(c.get() + layout.size() as u64));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { self.0.dealloc(ptr, layout) };
    }
}

/// Run `f` and return its result alongside the peak heap bytes allocated *above the baseline live at
/// entry* — the transient high-water mark of `f` itself, with pre-existing residency subtracted out.
///
/// Not reentrant and not thread-isolated: the counters are process-wide, so call it from a single
/// thread (e.g. one `#[test]`) with no other allocating work running concurrently.
pub fn peak_during<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    (out, peak)
}
