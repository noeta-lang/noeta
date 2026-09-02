//! The tree-walker's live-heap accounting — the eval-side half of the leak oracle
//! (architecture §0/§5) — and the eval half of the **safepoint-GC trigger**
//! (memory-management 6.x).
//!
//! The VM counts every [`noeta_value`] heap object directly (`noeta_value::live_count`). The
//! tree-walker reclaims through Rust `Rc`, so there is no single allocation chokepoint to
//! instrument; instead we count the heap-bearing aggregates that can *leak* — the ones that
//! participate in reference cycles: [`Scope`](crate::Scope), [`Closure`](crate::Closure), and
//! [`ObjectValue`](crate::ObjectValue). Each bumps this per-isolate (thread-local) counter when
//! constructed and drops it in its `Drop` impl. A program that reclaims cleanly returns the count
//! to its starting value; the known global-function ↔ global-scope cycle keeps a `Scope` (and its
//! `Closure`) alive past exit, so the oracle reports a positive residual for programs with
//! top-level functions — the debt the cycle collector drives to zero.
//!
//! The count is a signed integer so that a *missing* increment (a construction site that forgot to
//! route through the counted constructor) surfaces as a negative count rather than an unsigned
//! underflow — an under-count is self-revealing, while the `Drop` side is centralized and cannot be
//! missed.
//!
//! **Safepoint trigger.** The same counter doubles as the mid-run cycle-collection pressure gauge
//! (the eval mirror of `noeta_value`'s allocation watermark): [`inc`] sets a pending flag once the
//! count crosses a watermark, the interpreter's loop/call safepoints poll it, and
//! [`crate::cycles::safepoint_collect`] reclaims destructor-free cycle garbage. Armed per run,
//! geometric re-arm, disarmed for the exit reapers.

use std::cell::Cell;

thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
    /// High-water mark of [`LIVE`] since the last [`reset_peak`] — the peak-residency meter the
    /// bounded-residency regression test reads (the eval twin of `noeta_value::live_peak`).
    static PEAK: Cell<i64> = const { Cell::new(0) };
    /// Set when [`LIVE`] crossed [`WATERMARK`]; cleared by [`safepoint_rearm`].
    static PENDING: Cell<bool> = const { Cell::new(false) };
    /// The live count at which the next mid-run collection is requested. `i64::MAX` = disarmed.
    static WATERMARK: Cell<i64> = const { Cell::new(i64::MAX) };
    /// The configured growth step (see [`safepoint_arm`]); `i64::MAX` = disarmed.
    static STEP: Cell<i64> = const { Cell::new(i64::MAX) };
    /// Per-thread override of the arm step, for deterministic tests (`None` = process default).
    static STEP_OVERRIDE: Cell<Option<i64>> = const { Cell::new(None) };
}

/// The net number of live counted aggregates (`Scope` + `Closure` + `ObjectValue`) on this
/// isolate's thread. The leak oracle compares the per-program delta against zero.
pub fn live_count() -> i64 {
    LIVE.with(|c| c.get())
}

/// The peak live-aggregate count since the last [`reset_peak`] — the bounded-residency metric.
pub fn live_peak() -> i64 {
    PEAK.with(|c| c.get())
}

/// Reset the peak high-water mark to the current live count, so the next run's peak is measured
/// in isolation.
pub fn reset_peak() {
    PEAK.with(|p| LIVE.with(|l| p.set(l.get())));
}

/// Record the construction of a counted aggregate.
pub(crate) fn inc() {
    LIVE.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        PEAK.with(|p| {
            if n > p.get() {
                p.set(n);
            }
        });
        WATERMARK.with(|w| {
            if n >= w.get() {
                PENDING.with(|g| g.set(true));
            }
        });
    });
}

/// Record the reclamation of a counted aggregate (called from its `Drop`).
pub(crate) fn dec() {
    LIVE.with(|c| c.set(c.get() - 1));
}

/// Whether a mid-run collection has been requested on this thread (one thread-local bool read —
/// the interpreter safepoints' whole poll cost when idle).
#[inline]
pub(crate) fn safepoint_pending() -> bool {
    PENDING.with(|g| g.get())
}

/// Arm the safepoint trigger for a run: request a collection once `step` further aggregates are
/// live relative to now (a persistent session's existing residency is not charged).
pub(crate) fn safepoint_arm(step: i64) {
    STEP.with(|s| s.set(step));
    PENDING.with(|g| g.set(false));
    WATERMARK.with(|w| w.set(live_count().saturating_add(step)));
}

/// Disarm the trigger (exit reapers / teardown: destructors run against a heap being dismantled,
/// and the exit reap reclaims everything a pending safepoint would have).
pub(crate) fn safepoint_disarm() {
    STEP.with(|s| s.set(i64::MAX));
    PENDING.with(|g| g.set(false));
    WATERMARK.with(|w| w.set(i64::MAX));
}

/// Re-arm after a collection: watermark moves to `live + max(live, step)` — geometric growth, so
/// genuinely-live residency pays a vanishing collection frequency (the eval mirror of
/// `noeta_value::safepoint_gc_rearm`).
pub(crate) fn safepoint_rearm() {
    let step = STEP.with(|s| s.get());
    if step == i64::MAX {
        safepoint_disarm();
        return;
    }
    let live = live_count();
    WATERMARK.with(|w| w.set(live.saturating_add(live.max(step))));
    PENDING.with(|g| g.set(false));
}

/// The step a run arms with: the per-thread override if set (tests), else the process-wide
/// `NOETA_GC_THRESHOLD` (default 10k) — the same knob the VM reads, so one setting tunes both
/// backends. (Read here independently: the two backends deliberately share no runtime crate.)
pub(crate) fn safepoint_step() -> i64 {
    STEP_OVERRIDE.with(|o| o.get()).unwrap_or_else(|| {
        static FROM_ENV: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        *FROM_ENV.get_or_init(|| {
            std::env::var("NOETA_GC_THRESHOLD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10_000)
        })
    })
}

/// Override the arm step on this thread (`None` = back to the process default). Test seam: lets
/// the bounded-residency test force mid-run collections on a tiny heap deterministically.
pub fn set_safepoint_threshold(step: Option<i64>) {
    STEP_OVERRIDE.with(|o| o.set(step));
}
