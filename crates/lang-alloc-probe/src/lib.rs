//! A test-only heap-residency probe.
//!
//! [`TrackingAlloc`] is a pass-through [`GlobalAlloc`] wrapping the system allocator that maintains
//! two process-wide counters: the currently-live byte total and its all-time high-water mark.
//! Register it as the test binary's `#[global_allocator]` and bracket a region with [`peak_during`]
//! to learn the peak heap a piece of work touched. This is how the P-PACK memory-density wins are
//! measured (flat `Vec<u64>` buffers vs N boxed objects) — a number the time benchmarks cannot see.
//!
//! It is deliberately a standalone crate (depended on only as a `dev-dependency`) so that the
//! `unsafe` it needs to implement [`GlobalAlloc`] stays out of the production runtime crates, and so
//! the workspace `unsafe_code = "forbid"` quarantine is relaxed in exactly one extra, test-only place.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Currently-live bytes (sum of outstanding allocation sizes).
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// High-water mark of [`LIVE`] since the last [`peak_during`] reset.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// A `#[global_allocator]`-ready system allocator that tracks live bytes and their high-water mark.
/// The default [`GlobalAlloc::realloc`] is composed from `alloc`/`dealloc`, so resizes are counted.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackingAlloc;

// SAFETY: every call forwards directly to the system allocator with the same layout; the atomic
// bookkeeping only reads/writes counters and never touches the returned memory.
unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
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
