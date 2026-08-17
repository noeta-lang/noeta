//! The heap: manually-allocated, manually-refcounted objects behind a NaN-boxed pointer.
//!
//! This is the **only** module in the workspace that uses `unsafe`. It is kept small and
//! pure on purpose so `miri` can cover it: every `unsafe` here is either the int↔pointer
//! round-trip (via the exposed-provenance API, which is the `miri`-sound way to stash a
//! pointer in an integer) or a deref of an object this module itself allocated.
//!
//! An [`Obj`] is `Box`-allocated, its raw address stashed in a [`Value`] by the codec in
//! `lib.rs`, and freed by reconstructing the `Box`. Refcounts are non-atomic — the runtime
//! is shared-nothing per isolate, so no value crosses a thread boundary.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ptr;
use std::rc::Rc;

use noeta_ast::reflect::TypeRepr;
use noeta_bytecode::Builtin;
use noeta_object::{PackedSchema, Shape};

use crate::Value;
use noeta_ext_abi::MapKey;

// --- Live-heap accounting (the leak oracle, architecture §0/§5) ---
//
// A per-isolate (thread-local) count of live [`Obj`] allocations: bumped in [`alloc`], dropped in
// every reclamation path ([`free`], [`free_shallow`]). The runtime is shared-nothing per isolate so
// a thread-local is exactly per-isolate. The counter is the measuring stick for the leak oracle —
// `live_count()` must return to its pre-run value once a program's globals and frames are released
// (residency 0 at clean exit). It is a single integer, so it is always on (release builds pay one
// non-atomic increment per allocation).

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    /// High-water mark of [`LIVE`] since the last [`reset_peak`] — the peak-residency meter
    /// (architecture §0.3). Doubles the leak counter as a memory-footprint gauge: prompt last-use
    /// reclamation (Phase 3) should cut this materially vs the reclaim-at-teardown baseline.
    static PEAK: Cell<usize> = const { Cell::new(0) };
    /// The set of every live heap object on this thread, keyed by its NaN-boxed word — the
    /// **object registry** the Phase-6 backup mark-sweep collector ([`noeta_gc`]) sweeps. Updated on
    /// every [`alloc`] (insert) and every free ([`free`]/[`free_shallow`], remove). A cycle escapes
    /// refcounting but never the registry, so a trace from the live roots can find and reclaim it.
    /// (Always-on, like [`LIVE`]; an intrusive object-list is the perf option Phase 6.4 weighs.)
    /// Keyed with the fast [`FxHasher`], not the default SipHash — this set is hit on every alloc and
    /// free, and the keys are already well-distributed pointer words, so a crypto hash is pure cost.
    static REGISTRY: RefCell<RegistrySet> = RefCell::new(RegistrySet::default());
    /// Monotonic object-creation counter (object-model slice 2c) — stamps each allocation's
    /// `ObjHeader::seq` so the cycle collector can finalize in a deterministic age order.
    static NEXT_SEQ: Cell<u32> = const { Cell::new(0) };
}

/// The number of live heap objects on this isolate's thread. Zero at a clean program exit; the
/// leak oracle asserts the per-program delta is zero (a cycle leak or missed release shows up as a
/// positive residual).
pub fn live_count() -> usize {
    LIVE.with(|c| c.get())
}

thread_local! {
    /// Accumulated **refcount anomalies** noted by the backup trace collector (`noeta-gc`'s
    /// `collect_trace`) since the last [`reset_refcount_anomalies`]. The collector reclaims
    /// unreachable garbage — legitimate reference cycles and refcount-bug orphans alike — so plain
    /// end-of-run residency ([`live_count`]) can never see a skipped release/retain. This counter
    /// is the oracle's window into that blind spot: garbage is unreachable from the live graph, so
    /// its members can only be referenced from *within* the garbage set — in a refcount-correct
    /// program every member's count equals its in-edges from other members. A mismatch is a
    /// phantom (leaked) or missing (double-free-hazard) reference.
    static REFCOUNT_ANOMALIES: Cell<usize> = const { Cell::new(0) };
}

/// Add `n` refcount anomalies (the trace collector calls this per collection).
pub fn note_refcount_anomalies(n: usize) {
    REFCOUNT_ANOMALIES.with(|c| c.set(c.get() + n));
}

/// Reset the anomaly accumulator (the leak oracles call this before a measured run).
pub fn reset_refcount_anomalies() {
    REFCOUNT_ANOMALIES.with(|c| c.set(0));
}

/// The refcount anomalies accumulated since the last reset. Zero for a refcount-correct program;
/// the leak oracles assert it alongside residency.
pub fn refcount_anomalies() -> usize {
    REFCOUNT_ANOMALIES.with(|c| c.get())
}

/// The peak live-object count since the last [`reset_peak`] — the peak-residency metric.
pub fn live_peak() -> usize {
    PEAK.with(|c| c.get())
}

// --- The skipped-destructor audit ---------------------------------------------------------------
//
// Residency and refcount anomalies are both counts of *memory*, and there is a third way precise
// reference counting can be wrong that neither of them can see: an object whose type declares
// `destruct` is freed on a path that releases it without asking whether a destructor is due. Memory
// is reclaimed, residency returns to zero, and the destructor simply never runs — the program prints
// one line fewer than it should, which is a wrong answer no oracle comparing *memory* can produce.
// Nor can the differential see it: the two backends share the IR that decides where drops go, so a
// missing drop is missing in both and they agree, byte for byte, on the wrong output.
//
// The audit closes that: it counts the objects allocated with a destructor-bearing shape and the
// destructors actually run. At a clean exit — where residency is already asserted to be zero, so
// every object allocated has been freed — the two counts must be equal. A surplus allocation is an
// object that was freed with its `destruct` never run.
//
// Inert until [`destruct_audit_begin`], like the tree-walker's use-after-drop audit: one
// thread-local bool on the shaped-allocation path, nothing at all on the string/int path.

thread_local! {
    /// Whether the audit is recording. The allocation hook short-circuits on it, so a production
    /// run pays one bool read per shaped allocation.
    static DESTRUCT_AUDIT: Cell<bool> = const { Cell::new(false) };
    /// The names of the types whose declaration carries a `destruct` block, as the driver installs
    /// them at [`destruct_audit_begin`]. Empty is the common case and short-circuits the hook.
    static DESTRUCTIBLE_TYPES: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Objects allocated with a destructor-bearing shape since [`destruct_audit_begin`].
    static DESTRUCTIBLE_ALLOCS: Cell<usize> = const { Cell::new(0) };
    /// Destructors run since [`destruct_audit_begin`], as the runtime reports them.
    static DESTRUCTOR_RUNS: Cell<usize> = const { Cell::new(0) };
}

/// Start the skipped-destructor audit on this thread, recording against `types` — the names of the
/// types that declare a `destruct` block (the runtime's own destructor table, so the audit cannot
/// disagree with it about which types are in scope). Clears prior counts. Pair with
/// [`destruct_audit_end`].
pub fn destruct_audit_begin(types: HashSet<String>) {
    DESTRUCTIBLE_TYPES.with(|t| *t.borrow_mut() = types);
    DESTRUCTIBLE_ALLOCS.with(|c| c.set(0));
    DESTRUCTOR_RUNS.with(|c| c.set(0));
    DESTRUCT_AUDIT.with(|a| a.set(true));
}

/// Record that a destructor ran. Called by the runtime at its one destructor-invocation site.
#[inline]
pub fn note_destructor_run() {
    if DESTRUCT_AUDIT.with(|a| a.get()) {
        DESTRUCTOR_RUNS.with(|c| c.set(c.get() + 1));
    }
}

/// Stop the audit and return the number of **skipped destructors**: destructor-bearing objects
/// allocated minus destructors run. Zero for a correct program at a clean exit; a positive number
/// counts objects freed without their `destruct`. (Read it only where residency is also zero — a
/// still-live object has legitimately not been destructed yet.)
pub fn destruct_audit_end() -> i64 {
    DESTRUCT_AUDIT.with(|a| a.set(false));
    let allocs = DESTRUCTIBLE_ALLOCS.with(|c| c.get()) as i64;
    let runs = DESTRUCTOR_RUNS.with(|c| c.get()) as i64;
    DESTRUCTIBLE_TYPES.with(|t| t.borrow_mut().clear());
    allocs - runs
}

/// The audit's allocation hook: count an object whose shape names a destructor-bearing type.
#[inline]
fn note_destructible_alloc(payload: &Payload) {
    if !DESTRUCT_AUDIT.with(|a| a.get()) {
        return;
    }
    let Payload::Object { shape, .. } = payload else {
        return;
    };
    if DESTRUCTIBLE_TYPES.with(|t| t.borrow().contains(&shape.name)) {
        DESTRUCTIBLE_ALLOCS.with(|c| c.set(c.get() + 1));
    }
}

/// Reset the peak high-water mark to the current live count, so the next run's peak is measured in
/// isolation. Call before a measured run; read [`live_peak`] after.
pub fn reset_peak() {
    PEAK.with(|p| LIVE.with(|l| p.set(l.get())));
}

fn live_inc() {
    LIVE.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        PEAK.with(|p| {
            if n > p.get() {
                p.set(n);
            }
        });
        // Safepoint-GC trigger (memory-management 6.x): residency growth is allocation-driven, so
        // the allocation path is where the watermark is checked — the dispatch loop's poll then
        // reads one thread-local bool. Disarmed (`usize::MAX`) unless a run armed it.
        GC_WATERMARK.with(|w| {
            if n >= w.get() {
                GC_PENDING.with(|g| g.set(true));
            }
        });
    });
}

fn live_dec() {
    LIVE.with(|c| c.set(c.get() - 1));
}

/// A heap object: a refcount header followed by its payload. `repr(C)` so the header is
/// always first, though we only ever reach the payload through the typed `Box`.
#[repr(C)]
pub(crate) struct Obj {
    header: ObjHeader,
    /// The value's **reflected type tag** (runtime type-argument reflection, slice R1): the
    /// `TypeRepr` the checker resolved for this value's *construction site*, so `type_of` recovers a
    /// container's element type even after the value's static type was laundered through `dyn` — e.g.
    /// `type_of(launder([1,2,3]))` is `List(Int)`, not the head-only `List(Dyn)`. `None` for every
    /// value whose type is not carried (untagged) — a derived/mutated list, a scalar, an object today
    /// — which falls back to today's head-only runtime classification. It lives **beside** the payload
    /// (not inside it) precisely so it is invisible to value semantics: equality, hashing, `free`,
    /// `children`, and the COW fast paths all operate on `payload` and never see the tag. It is a leaf
    /// `Rc<TypeRepr>` (no child `Value`s), so it needs no GC handling — dropping the `Obj` drops it.
    reflect: Option<Rc<TypeRepr>>,
    pub(crate) payload: Payload,
}

#[repr(C)]
struct ObjHeader {
    /// Non-atomic: per-isolate single-threaded ownership (architecture §5, §7).
    refcount: u32,
    /// A monotonic per-isolate **creation sequence** (object-model slice 2c): assigned at
    /// allocation, it gives every object a stable, deterministic age. The cycle collector finalizes
    /// reclaimed members in reverse-creation order (newest-first), matching the language's
    /// reverse-declaration teardown — so cyclic `destruct` order is deterministic and agrees with the
    /// tree-walker (the live-object registry is a `HashSet`, whose iteration order is otherwise
    /// arbitrary).
    seq: u32,
    /// The trial-deletion cycle collector's per-object color (architecture §5). `Black` in
    /// normal use; the other colors are transient bookkeeping during a `collect`.
    color: Color,
    /// Whether the object is currently in the collector's candidate-root buffer.
    buffered: bool,
    /// **Borrow-share tag** (isolates I.3): a shared-immutable object promoted into a
    /// [`SharedRegion`], reachable read-only from other isolates. `retain`/`release` are **no-ops**
    /// on it — its refcount is never written, so no atomic ops and no cross-thread count race (the
    /// §7 borrow-not-refcount trick). Set **once** at promotion, before the graph is published to any
    /// worker, and never written again — so concurrent non-atomic *reads* of it are not a data race.
    /// Shared objects are owned solely by their region (freed wholesale at the scope join), so they
    /// live outside the refcount *and* the cycle collector (never registered, never buffered). `false`
    /// for every ordinary (local) object; single-isolate programs never set it.
    shared: bool,
    /// **Whether this object has an entry in the live-object [`REGISTRY`]** — the *recorded* answer
    /// to [`Payload::can_be_cyclic`], decided once in [`alloc_with`] and never rewritten. Every free
    /// path consults this bit rather than re-deriving the predicate, so the insert and the matching
    /// remove can never disagree: a payload mutated in place, or a collector mode flipped mid-run,
    /// would otherwise desync the set and leave the trace collector sweeping a freed address.
    /// Acyclic leaves are never registered — see [`Payload::can_be_cyclic`] for the invariant.
    registered: bool,
}

/// The cycle collector's object colors (Bacon–Rajan synchronous trial deletion). `Black` =
/// in use; `Gray` = under trial deletion; `White` = provisionally garbage; `Purple` = a
/// possible cycle root (an object whose count was decremented without reaching zero).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Black,
    Gray,
    White,
    Purple,
}

/// Which cycle collector the release path feeds (Phase 6.4 — the two are benchmarked head to head).
/// `Trace` (the default) keeps the live-object [`REGISTRY`] up to date for the backup mark-sweep and
/// frees promptly on every reclamation. `TrialDeletion` instead **buffers candidate roots** on a
/// surviving decrement and **defers the deallocation** of a buffered object that reaches refcount 0
/// (Bacon–Rajan), so the candidate buffer never dangles; it pays nothing per allocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CollectorMode {
    Trace,
    TrialDeletion,
}

thread_local! {
    /// The active collector for this isolate's release path. `Trace` by default (the proven Phase-6
    /// path); a benchmark / opt-in flips it to `TrialDeletion`.
    static MODE: Cell<CollectorMode> = const { Cell::new(CollectorMode::Trace) };
    /// The Bacon–Rajan candidate-root buffer (used only in `TrialDeletion` mode): objects whose
    /// refcount was decremented without reaching zero, hence possible cycle roots. Drained by
    /// `noeta_gc`'s trial-deletion collector.
    static CANDIDATES: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
}

/// Select the cycle collector the release path feeds. Set once before a run (the VM does this from
/// its configured mode); switching mid-run is not supported (the two keep different invariants).
pub fn set_collector_mode(mode: CollectorMode) {
    MODE.with(|m| m.set(mode));
}

// --- In-run safepoint-GC trigger (memory-management 6.x) ---
//
// Both cycle reapers historically ran only at clean exit, so a program building cycles in a loop
// had unbounded peak residency. The trigger below lets the VM run a collection DURING execution at
// a safepoint: the allocation path sets a `GC_PENDING` flag when the live count crosses a
// watermark (`Trace` mode) or when the candidate buffer crosses a floor (`TrialDeletion` mode),
// and the dispatch loop polls that one bool at loop back-edges and frame transfers. Thread-local,
// so every isolate carries its own trigger state — a worker collects at its own safepoints.

thread_local! {
    /// Set when a trigger condition crossed; cleared by [`safepoint_gc_rearm`] after a collection.
    static GC_PENDING: Cell<bool> = const { Cell::new(false) };
    /// The live-object count at which the next safepoint collection is requested (`Trace` mode).
    /// `usize::MAX` = disarmed (the default — plain `noeta-value` users never trigger).
    static GC_WATERMARK: Cell<usize> = const { Cell::new(usize::MAX) };
    /// The configured growth step: after a collection the watermark re-arms to
    /// `live + max(live, step)`, so collections amortize geometrically over residency growth.
    static GC_STEP: Cell<usize> = const { Cell::new(usize::MAX) };
    /// The candidate-buffer length at which the next collection is requested (`TrialDeletion`
    /// mode). `usize::MAX` = disarmed.
    static GC_CANDIDATE_FLOOR: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// The default safepoint-GC threshold: how many live heap objects (over the arm point) accumulate
/// before a mid-run collection is requested. Overridable per-process via `NOETA_GC_THRESHOLD`.
pub const SAFEPOINT_GC_DEFAULT_THRESHOLD: usize = 10_000;

/// The candidate-buffer growth (`TrialDeletion` mode) between safepoint collections.
const SAFEPOINT_GC_CANDIDATE_STEP: usize = 4_096;

/// The process-wide safepoint-GC threshold: `NOETA_GC_THRESHOLD` if set and parseable, else
/// [`SAFEPOINT_GC_DEFAULT_THRESHOLD`]. Read once.
pub fn safepoint_gc_default_threshold() -> usize {
    static FROM_ENV: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *FROM_ENV.get_or_init(|| {
        std::env::var("NOETA_GC_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(SAFEPOINT_GC_DEFAULT_THRESHOLD)
    })
}

/// Whether a safepoint collection has been requested on this thread (one thread-local bool read —
/// the dispatch loop's whole poll cost).
#[inline]
pub fn safepoint_gc_pending() -> bool {
    GC_PENDING.with(|g| g.get())
}

/// Arm the safepoint-GC trigger for a run: request a collection once `step` further objects are
/// live (relative to now, so a session/embed host's pre-existing residency is not charged), or —
/// in `TrialDeletion` mode — once the candidate buffer grows by its step. Called by a run entry;
/// thread-local, so each isolate arms its own.
pub fn safepoint_gc_arm(step: usize) {
    GC_STEP.with(|s| s.set(step));
    GC_PENDING.with(|g| g.set(false));
    GC_WATERMARK.with(|w| w.set(live_count().saturating_add(step)));
    GC_CANDIDATE_FLOOR.with(|f| {
        f.set(
            CANDIDATES
                .with(|c| c.borrow().len())
                .saturating_add(SAFEPOINT_GC_CANDIDATE_STEP),
        )
    });
}

/// Disarm the safepoint-GC trigger (tests / teardown hygiene): no further collections are
/// requested until the next [`safepoint_gc_arm`].
pub fn safepoint_gc_disarm() {
    GC_STEP.with(|s| s.set(usize::MAX));
    GC_PENDING.with(|g| g.set(false));
    GC_WATERMARK.with(|w| w.set(usize::MAX));
    GC_CANDIDATE_FLOOR.with(|f| f.set(usize::MAX));
}

/// Re-arm the trigger after a safepoint collection: clear the pending flag and move the watermark
/// to `live + max(live, step)` — geometric growth, so a program whose residency is genuinely live
/// pays a vanishing collection frequency, while a cycle-churning loop is collected every `step`
/// objects. The candidate floor re-arms relative to what stayed buffered (deferred components are
/// re-buffered, and must not immediately re-trigger).
pub fn safepoint_gc_rearm() {
    let step = GC_STEP.with(|s| s.get());
    if step == usize::MAX {
        safepoint_gc_disarm();
        return;
    }
    let live = live_count();
    GC_WATERMARK.with(|w| w.set(live.saturating_add(live.max(step))));
    GC_CANDIDATE_FLOOR.with(|f| {
        f.set(
            CANDIDATES
                .with(|c| c.borrow().len())
                .saturating_add(SAFEPOINT_GC_CANDIDATE_STEP),
        )
    });
    GC_PENDING.with(|g| g.set(false));
}

/// Re-buffer a value as a candidate cycle root (the safepoint trial-deletion collector's deferral
/// path: a destructor-bearing dead component is left allocated for the exit collection, which
/// finds it again through the buffer).
pub fn rebuffer_candidate(value: Value) {
    if value.is_pointer() {
        possible_root(value);
    }
}

/// The active collector mode.
pub fn collector_mode() -> CollectorMode {
    MODE.with(|m| m.get())
}

/// The heap payloads. Strings are the heap string type; `Int` boxes an `i64` that does
/// not fit the 48-bit immediate small-int range, so full i64 wrapping semantics are kept
/// (the differential oracle checks `i64::MAX + 1`). `Closure` holds a function-prototype
/// index into the compiled module's proto table plus the captured upvalue cells (see the
/// variant doc); a top-level function captures only globals (read live) and so has none.
///
/// `List` and `Map` are the M1.3 heap collections. A collection **owns one reference to
/// each value it holds** (the list's elements, the map's values; map keys are plain owned
/// `String`s, not values). When the collection is freed those owned references are released
/// first (see [`free`]), so dropping a list of strings frees the strings too. `BTreeMap`
/// gives the map deterministic, sorted-key iteration, matching the M0 tree-walker exactly.
/// `Object` and `Enum` are the M1.4 shaped aggregates. Each pairs a shared [`Shape`] handle
/// (the layout — same shape for same-built aggregates, so identity is a cheap `Rc` pointer
/// comparison) with a flat slot array it **owns one reference to each of**. An `Object`'s slots
/// A fast, deterministic string hasher (the `FxHash` rustc uses) for the map store. Rust's default
/// `HashMap` hasher is SipHash — DoS-resistant but ~3× slower on the short keys maps typically hold,
/// slower even than a `BTreeMap`'s fast-diverging comparisons. Iteration order is never observed
/// (every accessor sorts by key), so the hasher only needs speed, not randomness; this is
/// deterministic (seed 0). Not hash-flood-resistant — acceptable for a general runtime (PHP/Lua use
/// non-crypto hashes too), revisitable if adversarial input ever matters.
#[derive(Default)]
pub(crate) struct FxHasher {
    hash: u64,
}

const FX_K: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    /// **The** mixing round — the single primitive every `write_*` below is built from, so no
    /// writer can drift into its own variant.
    ///
    /// One widening multiply, halves folded (`mulx` + `xor` on x86-64). The classic Fx round is
    /// `(h.rotate_left(5) ^ word) * K`, and it is *not* good enough once the rounds are counted in
    /// words instead of bytes: a truncating multiply only ever propagates entropy **upward**, so the
    /// low bits of the result carry only the low bits of the input — and hashbrown indexes its
    /// buckets with exactly those low bits. Byte-at-a-time mixing hid that behind sheer round count
    /// (nine rounds for an eight-byte key); at two or three rounds it shows through as structured
    /// collisions. Measured on 4096 `"word{i}"` keys over 512 buckets: worst bucket 64 and 198
    /// buckets empty with the classic round, versus 20 and 0 here — the same spread the byte loop
    /// gave. Keeping the *whole* product is what makes one round per word sufficient.
    #[inline]
    fn mix(&mut self, word: u64) {
        let wide = u128::from(self.hash ^ word) * u128::from(FX_K);
        self.hash = (wide as u64) ^ ((wide >> 64) as u64);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }

    /// Word-at-a-time. This mixed **one byte per round** until now, so an eight-byte map key cost
    /// eight serially-dependent multiplies where one does — and that dependency chain, not the
    /// instruction count, is what put hashing at 2-3% of the `assoc`/`wordcount` profiles.
    ///
    /// The length goes in first because the tail is zero-padded into a word: without it `"ab"` and
    /// `"ab\0\0\0\0\0\0"` would mix identically. (The old byte loop needed no length — it had no
    /// padding to be confused by.)
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.mix(bytes.len() as u64);
        let mut chunks = bytes.chunks_exact(8);
        for chunk in chunks.by_ref() {
            self.mix(u64::from_le_bytes(
                chunk.try_into().expect("chunks_exact(8) yields 8 bytes"),
            ));
        }
        let tail = chunks.remainder();
        if !tail.is_empty() {
            let mut word = [0u8; 8];
            word[..tail.len()].copy_from_slice(tail);
            self.mix(u64::from_le_bytes(word));
        }
    }

    // A whole-word fast path for `u64` keys (the live-object registry keys on the NaN-boxed pointer
    // word) — one round instead of eight byte rounds, and no SipHash.
    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.mix(n);
    }

    /// One round for a single byte, rather than routing through [`Self::write`]'s length-plus-tail
    /// pair. `str`'s own `Hash` ends with a `write_u8(0xff)` terminator, so this sits on the hot
    /// path of every string key. It deliberately does **not** agree with `write(&[n])` — nothing
    /// hashes one byte both ways, and `Hasher` requires only that each method be deterministic.
    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.mix(n as u64);
    }
}

/// The map store: a hashbrown `HashMap` with the fast [`FxHasher`] (see above), so
/// get/insert/remove are O(1) and cheap. Keyed by the shared [`MapKey`] (extern-types X4):
/// string keys keep their exact P-SSO representation and hash (content-only), and the bare
/// `&str` probe still allocates nothing (hashbrown's `Equivalent` lookup); extern keys ride the
/// same table. Aliased so the type appears in exactly one place. (hashbrown IS std's table —
/// taken directly for the heterogeneous-lookup API std does not expose.)
pub(crate) type MapStore =
    hashbrown::HashMap<MapKey, Value, std::hash::BuildHasherDefault<FxHasher>>;

/// The live-object registry set — a `HashSet<u64>` (NaN-boxed object words) with the fast
/// [`FxHasher`] instead of SipHash, since it is touched on every alloc and free.
type RegistrySet = HashSet<u64, std::hash::BuildHasherDefault<FxHasher>>;

/// are its field values in the shape's declared order; an `Enum`'s slots are its variant's
/// positional data. Freeing either releases its slots first (see [`free`]).
pub(crate) enum Payload {
    Str(compact_str::CompactString),
    /// A raw immutable byte buffer (`bytes`, P-PACK 4.4) — a GC leaf like `Str`; owns no child
    /// references, freeing it just drops the `Vec<u8>`.
    Bytes(Vec<u8>),
    /// A registered extern-type value (extern-types X1) — the ONE hosting variant every
    /// registry-contributed type shares. A GC leaf: the contract is acyclic by design (no child
    /// `Value`s), so freeing just drops the box. The payload is RC-shared like any other, so a
    /// mutating method (through [`with_extern_mut`]) has reference semantics — the FileHandle
    /// discipline, generalized.
    Extern(noeta_ext_abi::ExternBox),
    Int(i64),
    /// A function value: a prototype index plus the captured upvalue cells (empty for a
    /// top-level `fn`/closure, which captures only globals). The closure **owns one reference
    /// to each upvalue cell**, so it is a GC node — a closure capturing a cell that captures
    /// the closure forms a cycle the trial-deletion collector reclaims.
    Closure {
        proto: u32,
        upvalues: Vec<Value>,
    },
    /// A mutable single-slot box: the shared storage for a local that an inner closure
    /// captures. It **owns one reference** to the value it holds (released in [`free`], traced
    /// by [`children`]), so the defining frame and every capturing closure see one live binding.
    Cell(Value),
    List(Vec<Value>),
    /// A tuple: a fixed-arity, heterogeneous, value-semantic positional aggregate (object-model
    /// slice 4). Stored as its element vector, owning one reference to each element exactly like a
    /// list — equality is structural and there is no shape (arity/positions are static, checked at
    /// compile time, so no per-value metadata is needed).
    Tuple(Vec<Value>),
    /// A set, stored as its canonical (sorted, de-duplicated) element vector — so iteration,
    /// display, and equality are deterministic and identical to the tree-walker. It owns one
    /// reference to each element, freed like a list's.
    Set(Vec<Value>),
    // A `HashMap` for O(1) get/insert/remove (the hot path). Iteration order is unobservable: every
    // order-observing accessor (`map_keys`/`map_values`/`map_entries`/`repr`/`to_native_deep`) sorts
    // by key, so maps still present and compare in deterministic sorted order (differential-safe).
    Map(MapStore),
    /// A flat `List<packed>` (P-PACK 2.4, byte-addressed since 3.2b): the elements packed as raw
    /// primitive bytes, one contiguous `Vec<u8>` of `schema.byte_size` bytes per element (an `f32`
    /// field is 4 bytes, the others 8), interpreted through the shared `schema`. A GC **leaf** — it
    /// owns no child `Value`s (only primitive bytes), so freeing it just drops the buffer. Elements
    /// are materialized to/from `Payload::Object` on demand, so the layout is invisible to `RunResult`.
    PackedList {
        schema: &'static PackedSchema,
        bytes: Vec<u8>,
    },
    Object {
        shape: &'static Shape,
        slots: Vec<Value>,
    },
    Enum {
        shape: &'static Shape,
        data: Vec<Value>,
    },
    /// A Ring 2 native module (`use std.{json}`), identified by its surface name. A leaf with
    /// no child values; dispatched by `noeta-vm` (which maps the name to the module).
    NativeModule(String),
    /// A first-class prelude builtin (`len`/`map`/`filter`/`sum`) used as a value. A leaf (the
    /// `Builtin` id is plain data); `noeta-vm` dispatches it at an indirect call site.
    NativeFn(Builtin),
    /// A selectively-imported native-module function (`use std.math.sqrt` → bare `sqrt`) used as a
    /// value or called directly. A leaf (two owned `String`s, no child values); dispatched through
    /// the same `call_native_module` path as a `<module>.<func>` member call, so the two backends
    /// agree by construction.
    ModuleFn {
        module: String,
        func: String,
    },
    /// An unbound method handle (`Type.method` as a value). When called it dispatches by name — as
    /// an instance method on its first argument (`associated == false`), or as an associated call
    /// `ty.method(args)` (`associated == true`). A leaf (owned `String`s + a bool). Both backends
    /// dispatch through the shared method machinery, so they agree by construction.
    MethodHandle {
        ty: String,
        method: String,
        associated: bool,
    },
    /// A **bound** method handle (`value.method` as a value, prelude-redesign EX.2b): the receiver
    /// is captured at bind time (one owned reference — for a `class` the shared instance, so later
    /// mutations are visible through the handle; for a value type a value-semantic copy). Calling it
    /// dispatches `method` on the captured receiver. NOT a leaf: `recv` is a child value (traversed
    /// by the cycle collector, released with the handle).
    BoundMethod {
        recv: Value,
        method: String,
    },
    /// A lazy iterator (Track I): a reference-semantic pull cursor. The base (`iter()`) is a cursor
    /// over a list; adapters (`take`/`drop`/`chain`/…) wrap one or two **source** iterators and pull
    /// from them on demand, so a pipeline fuses with no intermediate list. It **owns one reference**
    /// to each source it holds (a GC node like [`Self::Cell`]); the cursor mutation through `next()`
    /// is shared by every alias, exactly like a file handle. See [`IterState`].
    Iter(IterState),
    /// An async future (Track A): a reference-semantic deferred computation. In A.1 it wraps a **lazy
    /// thunk** — a zero-argument closure that runs the `async fn` body and returns the completion value
    /// — which is not invoked until the future is awaited/run (Rust-style laziness). Owns **one
    /// reference** to that closure (a GC node like [`Self::Cell`]). A.2 replaces the thunk with the
    /// async state machine and this becomes a pollable future.
    Future(Value),
    /// A **leaf timer future** (Track A.2): `sleep(ms)` produces one, carrying the absolute logical
    /// deadline (ms) at which it becomes ready. It holds no heap children — the deadline is a plain
    /// integer — so it needs no `release`/`children` handling beyond freeing its own node. Polling it
    /// consults the injected [`noeta_ext_abi::SandboxExecutor`]'s clock; it reports `Pending` until the
    /// clock reaches the deadline. This is the first future that can actually suspend.
    Timer(u64),
    /// A **task handle** (Track A.3b): the `Future<T>` `spawn e` returns. It references a task by its
    /// [`ScopeId`]/[`TaskId`] position in the backend's concurrency-scope stack; polling it reads
    /// the task's stored result (ready) or reports pending. A GC leaf — the two indices are plain
    /// integers; the task's future/result are owned by the scope, not the handle.
    Handle(crate::ScopeId, crate::TaskId),
    /// A **leaf async-read future** (Track A.4c): the `Future<string>` `fs.read_async(path)` returns.
    /// It carries an id ticketing the read in the injected [`noeta_ext_abi::Executor`] (the sandbox
    /// executor resolves it synchronously; the real executor spawns it on tokio and harvests it in
    /// `advance`). A GC leaf — the id is a plain integer; the pending read lives in the executor.
    AsyncIo(u64),
    /// A **channel sender endpoint** (isolates I.1): the `Sender<T>` `channel::<T>(cap)` yields. It
    /// carries the channel's [`ChannelId`](crate::ChannelId) into the backend's channel table;
    /// `tx.send(v)`/`tx.close()` dispatch on it. A GC leaf — the id is a plain integer; the queue
    /// lives in the backend.
    Sender(crate::ChannelId),
    /// A **channel receiver endpoint** (isolates I.1): the `Receiver<T>` `channel::<T>(cap)` yields.
    /// A GC leaf like [`Self::Sender`]; `rx.recv()` dispatches on it.
    Receiver(crate::ChannelId),
    /// A **leaf channel-send future** (isolates I.1): `tx.send(v)` produces one, carrying the channel
    /// id and **owning one reference** to the message `v` (a GC node like [`Self::Cell`]). Polling it
    /// enqueues `v` when the buffer has room (ready → unit) or reports pending on a full buffer. The
    /// third word is its capacity-0 **rendezvous phase** (isolates I.4c) — whether it has yet
    /// deposited into the one-slot handoff; ignored for a buffered channel.
    ChannelSend(crate::ChannelId, Value, noeta_ext_abi::channel::SendPhase),
    /// A **leaf channel-recv future** (isolates I.1): `rx.recv()` produces one, carrying the channel
    /// id. Polling it dequeues the next message (ready → `some(v)`), reports `none` once closed and
    /// drained, or pending on an empty open buffer. A GC leaf — the queued messages live in the backend.
    ChannelRecv(crate::ChannelId),
    /// A **leaf isolate-result future** (isolates I.4b): the `Future<T>` a real-thread `isolate f(args)`
    /// yields, carrying an id into the backend's isolate table (the worker thread's join handle + result
    /// receiver). Polling it harvests the marshalled result once the worker finishes, else pending. A GC
    /// leaf — the id is a plain integer; the worker's state lives in the backend. VM-real path only.
    IsolateFuture(u32),
    // (`Reactive` lived here until higher-order-abi H5 — the handles are registry extern types
    // now, their contents in the extensions' retained arena.)
}

impl Payload {
    /// **THE cycle-participation predicate**: can a value with this payload take part in a reference
    /// cycle? Exactly the payloads that *own a child `Value`* — the GC **nodes**. Everything else is a
    /// **leaf**: its bytes are primitives or owned Rust data, it references no other heap object, so
    /// no chain of references can ever return to it.
    ///
    /// This is the single source of truth for two consumers, which is the whole point of stating it
    /// once: the allocation path (whether to put the object in the live-object [`REGISTRY`] the backup
    /// mark-sweep sweeps) and the release path (whether to buffer a surviving decrement as a
    /// Bacon–Rajan candidate cycle root). Both questions are the same question.
    ///
    /// **Why excluding leaves preserves the collectors' invariants.** The registry exists so a
    /// collection can find objects that are *allocated but unreachable* — refcounting alone cannot
    /// reclaim those. An object is allocated-but-unreachable only if every owner of it is itself
    /// unreachable; following owners upward that regress must terminate in a cycle, so every such
    /// object is either a cycle member or is *reached from* one. A cycle member holds a reference to
    /// the next member, hence is a node, hence is registered. A leaf held (only) by dead nodes is
    /// reclaimed without the registry: the trace hands the dead nodes back and the reclaim releases
    /// each dead node's edges to non-dead values, dropping the leaf's count to zero and freeing it
    /// promptly; the trial-deletion collector reaches it through `gc_children` and its trial
    /// decrement, never through the registry. So no leaf needs an entry, and the sole behavior a leaf
    /// entry ever bought was mopping up a leaf leaked by a *refcount bug* — which the leak oracle
    /// still catches as non-zero end-of-run residency ([`live_count`], which leaves do still bump).
    ///
    /// The arms mirror [`children`] and [`free`] one-for-one, and `children` `debug_assert`s the
    /// coupling (anything that yields a child must answer `true` here), so a new node variant that
    /// forgets this predicate fails a debug run rather than silently escaping the collector.
    fn can_be_cyclic(&self) -> bool {
        match self {
            // Nodes: own one reference to each child value.
            Payload::List(_)
            | Payload::Tuple(_)
            | Payload::Set(_)
            | Payload::Map(_)
            | Payload::Object { .. }
            | Payload::Enum { .. }
            | Payload::Closure { .. }
            | Payload::Cell(_)
            | Payload::Iter(_)
            | Payload::Future(_)
            | Payload::ChannelSend(..)
            | Payload::BoundMethod { .. } => true,
            // Leaves: primitives, owned Rust data, or plain ids — no child `Value`, ever.
            Payload::Str(_)
            | Payload::Bytes(_)
            | Payload::Extern(_)
            | Payload::Int(_)
            | Payload::PackedList { .. }
            | Payload::NativeModule(_)
            | Payload::NativeFn(_)
            | Payload::ModuleFn { .. }
            | Payload::MethodHandle { .. }
            | Payload::Timer(_)
            | Payload::Handle(..)
            | Payload::AsyncIo(_)
            | Payload::Sender(_)
            | Payload::Receiver(_)
            | Payload::ChannelRecv(_)
            | Payload::IsolateFuture(_) => false,
        }
    }
}

/// The state machine behind a [`Payload::Iter`] (Track I). The base case cursors a list; each adapter
/// holds the source iterator(s) it pulls from. `noeta-eval` mirrors this enum over its own `Value`.
pub(crate) enum IterState {
    /// Cursor over a backing list — the base iterator from `iter()`.
    List { list: Value, cursor: usize },
    /// Yield at most `remaining` more elements from `source` (`take(n)`).
    Take { source: Value, remaining: usize },
    /// Skip `pending` elements from `source`, then yield the rest (`drop(n)`).
    Drop { source: Value, pending: usize },
    /// Yield all of `first`, then all of `second` (`chain(other)`).
    Chain { first: Value, second: Value },
    /// Yield `(index, element)` tuples from `source`, indexing from `index` (`enumerate()`).
    Enumerate { source: Value, index: usize },
    /// Yield `(a_elem, b_elem)` tuples, stopping when either runs dry (`zip(other)`).
    Zip { a: Value, b: Value },
    /// Yield `func(element)` for each element of `source` (`map(f)`, Track I.1c). Owns a reference to
    /// the closure `func` so it stays alive for the iterator's lifetime.
    Map { source: Value, func: Value },
    /// Yield the elements of `source` for which `pred(element)` is true (`filter(f)`, Track I.1c).
    Filter { source: Value, pred: Value },
    /// A generator (Track G): `step` is a closure (a state machine over `mut`-captured cells) called
    /// once per element with one resume argument, returning `?T` (`some(x)` → element, `none` → end).
    /// Owns one reference to the closure.
    Gen { step: Value },
}

/// A snapshot of an [`IterState`]'s shape, with its child [`Value`]s (Copy) and counters copied out.
/// Reading this under a *short* borrow lets the pull driver recurse into a source — or run a user
/// closure — with **no** borrow held on the node, so a re-entrant access to the same iterator cannot
/// alias the live `&mut` (which would be undefined behavior). See [`crate::Value::iter_next_apply`].
pub(crate) enum IterShape {
    List,
    Take { source: Value, remaining: usize },
    Drop { source: Value, pending: usize },
    Chain { first: Value, second: Value },
    Enumerate { source: Value, index: usize },
    Zip { a: Value, b: Value },
    Map { source: Value, func: Value },
    Filter { source: Value, pred: Value },
    Gen { step: Value },
}

impl IterState {
    /// The child iterator/list/closure values this state owns one reference to (for GC trace/free).
    fn children(&self) -> [Option<Value>; 2] {
        match self {
            IterState::List { list, .. } => [Some(*list), None],
            IterState::Take { source, .. }
            | IterState::Drop { source, .. }
            | IterState::Enumerate { source, .. } => [Some(*source), None],
            IterState::Chain { first, second } => [Some(*first), Some(*second)],
            IterState::Zip { a, b } => [Some(*a), Some(*b)],
            IterState::Map { source, func } => [Some(*source), Some(*func)],
            IterState::Filter { source, pred } => [Some(*source), Some(*pred)],
            IterState::Gen { step } => [Some(*step), None],
        }
    }

    /// Copy this state's shape out (child values + counters) so the caller can act without holding a
    /// borrow on the node. `List` carries no copy — it has no recursion or user code, so its cursor is
    /// advanced under its own short borrow.
    pub(crate) fn shape(&self) -> IterShape {
        match self {
            IterState::List { .. } => IterShape::List,
            IterState::Take { source, remaining } => IterShape::Take {
                source: *source,
                remaining: *remaining,
            },
            IterState::Drop { source, pending } => IterShape::Drop {
                source: *source,
                pending: *pending,
            },
            IterState::Chain { first, second } => IterShape::Chain {
                first: *first,
                second: *second,
            },
            IterState::Enumerate { source, index } => IterShape::Enumerate {
                source: *source,
                index: *index,
            },
            IterState::Zip { a, b } => IterShape::Zip { a: *a, b: *b },
            IterState::Map { source, func } => IterShape::Map {
                source: *source,
                func: *func,
            },
            IterState::Filter { source, pred } => IterShape::Filter {
                source: *source,
                pred: *pred,
            },
            IterState::Gen { step } => IterShape::Gen { step: *step },
        }
    }
}

/// Allocate an object and return a NaN-boxed pointer [`Value`] owning one reference. `shared` sets
/// the header's shared-immutable tag (isolates I.3) and, when set, keeps the object **out** of the
/// cycle-collector [`REGISTRY`]: a shared object is owned by a [`SharedRegion`] and freed wholesale at
/// the scope join — never by refcount, never by the GC — so it must stay out of the collector's
/// world (it still counts toward [`live_count`], so the leak oracle sees the region balance). A normal
/// object registers as a mark-sweep candidate in `Trace` mode.
fn alloc_with(payload: Payload, shared: bool) -> Value {
    live_inc();
    note_destructible_alloc(&payload);
    let seq = NEXT_SEQ.with(|c| {
        let s = c.get();
        c.set(s.wrapping_add(1));
        s
    });
    // The registry decision, taken **once**, here: only a GC node in `Trace` mode is swept, and a
    // shared object is never GC-managed at all. Answering the cheap payload predicate first keeps a
    // string/int allocation off both thread-locals entirely — the whole point of the exclusion.
    let registered = payload.can_be_cyclic() && !shared && collector_mode() == CollectorMode::Trace;
    let raw = Box::into_raw(Box::new(Obj {
        header: ObjHeader {
            refcount: 1,
            seq,
            color: Color::Black,
            buffered: false,
            shared,
            registered,
        },
        reflect: None,
        payload,
    }));
    let addr = raw.expose_provenance();
    debug_assert!(
        addr & !Value::PTR_MASK as usize == 0,
        "heap address does not fit the 48-bit NaN-box payload"
    );
    let value = Value(Value::SIGN_BIT | Value::QNAN | (addr as u64 & Value::PTR_MASK));
    // The registry is the backup mark-sweep's sweep set; trial-deletion works from buffered candidates
    // instead, so it pays no per-allocation registry cost (the Phase-6.4 trade-off).
    if registered {
        REGISTRY.with(|r| r.borrow_mut().insert(value.0));
    }
    value
}

/// Allocate an object and return a NaN-boxed pointer [`Value`] owning one reference.
pub(crate) fn alloc(payload: Payload) -> Value {
    alloc_with(payload, false)
}

/// Allocate a **shared-immutable** object (isolates I.3) — see [`alloc_with`]. Used only by
/// [`SharedRegion`].
fn alloc_shared(payload: Payload) -> Value {
    alloc_with(payload, true)
}

/// Drop `value` from the live-object registry — called by every free path so the registry tracks
/// exactly the registered live heap. Separate from the `live_dec` counter so both stay in lock-step.
///
/// `was_registered` is the object's own [`ObjHeader::registered`] bit, read out of the box being
/// freed: the *recorded* answer from [`alloc_with`], never a fresh derivation. That is what makes the
/// insert and the remove symmetric by construction — an unregistered object (an acyclic leaf, a
/// shared object, anything allocated in `TrialDeletion` mode) never touches the set on either end.
fn registry_remove(value: Value, was_registered: bool) {
    if was_registered {
        REGISTRY.with(|r| r.borrow_mut().remove(&value.0));
    }
}

/// A snapshot of every live heap object, for the backup mark-sweep collector. Reconstructs a
/// [`Value`] from each registered word (a pure bit-reinterpret; the provenance was exposed at
/// [`alloc`], so recovering the pointer later is sound). The borrow is released before the caller
/// sweeps, so freeing (which mutates the registry) cannot alias it.
pub fn live_objects() -> Vec<Value> {
    REGISTRY.with(|r| r.borrow().iter().map(|&w| Value(w)).collect())
}

/// Recover the typed pointer from a NaN-boxed pointer value. The caller must have checked
/// `value.is_pointer()`.
fn obj_ptr(value: Value) -> *mut Obj {
    let addr = (value.0 & Value::PTR_MASK) as usize;
    ptr::with_exposed_provenance_mut::<Obj>(addr)
}

/// Read the payload of a pointer value under a closure, so no reference outlives the
/// object. The caller must have checked `value.is_pointer()`.
pub(crate) fn with_payload<R>(value: Value, f: impl FnOnce(&Payload) -> R) -> R {
    // SAFETY: `value` is a pointer this module allocated and has not freed (refcount > 0),
    // so the object is live and the reference does not escape the closure.
    let obj = unsafe { &*obj_ptr(value) };
    f(&obj.payload)
}

/// Mutate the payload of a pointer value under a closure, so no reference outlives the object.
/// The caller must have checked `value.is_pointer()` **and** must hold the only owning reference
/// (refcount == 1) when the mutation observably changes the value — this is the in-place
/// copy-on-write path. Single-threaded, so the `&mut` is unaliased.
pub(crate) fn with_payload_mut<R>(value: Value, f: impl FnOnce(&mut Payload) -> R) -> R {
    // SAFETY: `value` is a live pointer this module allocated; single-threaded, and the caller
    // guarantees uniqueness for the COW case, so the `&mut` does not alias another live reference.
    let obj = unsafe { &mut *obj_ptr(value) };
    f(&mut obj.payload)
}

/// The value's reflected type tag (slice R1), or `None` if untagged. A cheap `Rc` clone (refcount
/// bump). The caller must have checked `value.is_pointer()`.
pub(crate) fn reflect(value: Value) -> Option<Rc<TypeRepr>> {
    // SAFETY: live object allocated by this module; single-threaded read.
    let obj = unsafe { &*obj_ptr(value) };
    obj.reflect.clone()
}

/// Set (or clear) the value's reflected type tag (slice R1). Used at list-literal construction to
/// stamp the checker-resolved element type, and to **clear** the tag on an in-place COW mutation (so
/// a reused list node does not carry the original literal's type through a value-producing op — the
/// tag survives pure aliasing only, refcount-independently). The caller must hold `value.is_pointer()`.
pub(crate) fn set_reflect(value: Value, tag: Option<Rc<TypeRepr>>) {
    // SAFETY: live object allocated by this module; single-threaded write to a field the value
    // semantics never observe (equality/hash/free/children all read `payload`, never `reflect`).
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.reflect = tag;
}

/// Read the current refcount of a pointer value. Used to detect the last reference (so a
/// destructor can run on the about-to-be-final release). The caller must have checked
/// `value.is_pointer()`.
pub(crate) fn refcount(value: Value) -> u32 {
    // SAFETY: live object allocated by this module; single-threaded read.
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.refcount
}

/// The object's creation sequence (object-model slice 2c) — its allocation age, for the cycle
/// collector's deterministic reverse-creation finalization order.
pub(crate) fn seq(value: Value) -> u32 {
    // SAFETY: live object allocated by this module; single-threaded read.
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.seq
}

/// Whether a pointer value is a **shared-immutable** (borrow-shared) object (isolates I.3). The
/// caller must have checked `value.is_pointer()`. A shared object's refcount is never written.
pub(crate) fn is_shared(value: Value) -> bool {
    // SAFETY: live object allocated by this module; single-threaded read of a write-once flag.
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.shared
}

/// Increment the refcount of a pointer value. No-op enforced by the caller for immediates, and a
/// no-op on a **shared** object (isolates I.3): a borrow-shared graph is never refcounted, so no
/// count is written and nothing races across isolates.
pub(crate) fn inc_ref(value: Value) {
    // SAFETY: live object allocated by this module; single-threaded so the read-modify-write
    // is not racy.
    let obj = unsafe { &mut *obj_ptr(value) };
    if obj.header.shared {
        return;
    }
    obj.header.refcount += 1;
}

/// Decrement the refcount; return `true` when it reaches zero (the caller then [`free`]s). A no-op
/// returning `false` on a **shared** object (isolates I.3) — a borrow-shared object is freed
/// wholesale by its region, never at a refcount of zero, so its count is left untouched.
pub(crate) fn dec_ref(value: Value) -> bool {
    // SAFETY: as `inc_ref`.
    let obj = unsafe { &mut *obj_ptr(value) };
    if obj.header.shared {
        return false;
    }
    obj.header.refcount -= 1;
    obj.header.refcount == 0
}

/// Free a pointer value whose refcount has reached zero, running the payload's destructor
/// (e.g. the `String`'s) by reconstructing and dropping the `Box`. A collection owns one
/// reference to each value it holds, so those are released first (which recursively frees
/// any that reach zero) before the container's own allocation is dropped.
pub(crate) fn free(value: Value) {
    // SAFETY: `value` is a pointer this module allocated, its refcount is zero (so no other
    // owner exists), and it is freed exactly once.
    let boxed = unsafe { Box::from_raw(obj_ptr(value)) };
    registry_remove(value, boxed.header.registered);
    live_dec();
    match &boxed.payload {
        Payload::List(items)
        | Payload::Tuple(items)
        | Payload::Set(items)
        | Payload::Object { slots: items, .. }
        | Payload::Enum { data: items, .. } => {
            for &element in items {
                release_child(element);
            }
        }
        Payload::Map(entries) => {
            for &element in entries.values() {
                release_child(element);
            }
        }
        Payload::Closure { upvalues, .. } => {
            for &cell in upvalues {
                release_child(cell);
            }
        }
        Payload::Cell(inner) => release_child(*inner),
        // An iterator owns one reference to each source it holds (a node like `Cell`).
        Payload::Iter(state) => {
            for child in state.children().into_iter().flatten() {
                release_child(child);
            }
        }
        // A future owns one reference to its thunk/step closure (a node like `Cell`).
        Payload::Future(step) => release_child(*step),
        // A channel-send future owns the message it is queuing until it is enqueued or dropped.
        Payload::ChannelSend(_, value, _) => release_child(*value),
        // A bound method handle owns its captured receiver.
        Payload::BoundMethod { recv, .. } => release_child(*recv),
        // A packed list (P-PACK 2.4) owns only primitive words — no child references — so freeing it
        // just drops the buffer (and its shared `Rc<PackedSchema>`), like any other leaf.
        Payload::Str(_)
        | Payload::Bytes(_)
        | Payload::Int(_)
        | Payload::NativeModule(_)
        | Payload::NativeFn(_)
        | Payload::ModuleFn { .. }
        | Payload::MethodHandle { .. }
        | Payload::PackedList { .. }
        | Payload::Timer(_)
        | Payload::Handle(..)
        | Payload::AsyncIo(_)
        | Payload::Sender(_)
        | Payload::Receiver(_)
        | Payload::ChannelRecv(_)
        | Payload::IsolateFuture(_)
        | Payload::Extern(_) => {}
    }
    drop(boxed);
}

/// Drop one owned reference to a value a freed container held, reclaiming it at zero. The child is a
/// distinct object from the container being freed, so there is no aliasing. Routes through the
/// mode-aware [`release`] so a freed container's children feed the active collector (buffering cycle
/// roots in `TrialDeletion` mode just as the top-level release does).
fn release_child(value: Value) {
    release(value);
}

/// Drop one owning reference to `value`, reclaiming it through the **active collector** (Phase 6.4).
/// In `Trace` mode this is the prompt refcount free; in `TrialDeletion` mode it is the Bacon–Rajan
/// `Decrement` — a surviving decrement buffers a possible cycle root, and an object reaching zero
/// releases its children immediately but **defers its own deallocation if it is buffered** (so the
/// candidate buffer never holds a freed pointer). A no-op for immediates.
pub(crate) fn release(value: Value) {
    if !value.is_pointer() {
        return;
    }
    // A borrow-shared object (isolates I.3) is owned by its region and freed wholesale at the scope
    // join — release touches neither its count nor the cycle collector, so there is nothing to race.
    if is_shared(value) {
        return;
    }
    match MODE.with(|m| m.get()) {
        CollectorMode::Trace => {
            if dec_ref(value) {
                free(value);
            }
        }
        CollectorMode::TrialDeletion => {
            if dec_ref(value) {
                // Bacon–Rajan `Release`: release children now (their own decrements may buffer them
                // as roots), then reclaim this object's allocation. `free_shallow` defers it if it is
                // buffered, so the buffer's reference stays valid until the collector frees it.
                for child in children(value) {
                    release(child);
                }
                free_shallow(value);
            } else if can_be_cyclic(value) {
                possible_root(value);
            }
        }
    }
}

/// Whether a *value* can participate in a cycle — the [`Payload::can_be_cyclic`] predicate applied
/// through the NaN box (an immediate has no heap identity, so it never can). The release path buffers
/// only these as Bacon–Rajan candidate roots; the allocation path registers only these with the
/// backup mark-sweep. One predicate, both paths.
fn can_be_cyclic(value: Value) -> bool {
    if !value.is_pointer() {
        return false;
    }
    // SAFETY: pointer-tag checked above; a live pointer this module allocated (callers hold a
    // reference, so it is unfreed); single-threaded read.
    let obj = unsafe { &*obj_ptr(value) };
    obj.payload.can_be_cyclic()
}

/// Buffer `value` as a Bacon–Rajan possible cycle root (`PossibleRoot`): paint it purple and, if it
/// is not already buffered, record it for the next trial-deletion collection.
fn possible_root(value: Value) {
    if color(value) != Color::Purple {
        set_color(value, Color::Purple);
        if !buffered(value) {
            set_buffered(value, true);
            let len = CANDIDATES.with(|c| {
                let mut c = c.borrow_mut();
                c.push(value);
                c.len()
            });
            // Safepoint-GC trigger, `TrialDeletion` mode: the buffer crossing its floor requests a
            // mid-run collection (the buffer IS the collector's whole input, so its growth — not
            // raw allocation — is the right pressure signal here). Disarmed floor = `usize::MAX`.
            if len >= GC_CANDIDATE_FLOOR.with(|f| f.get()) {
                GC_PENDING.with(|g| g.set(true));
            }
        }
    }
}

/// Take the buffered candidate roots for a trial-deletion collection (drained, so the next round
/// starts empty). The borrow is released before the collector touches the objects.
pub fn take_candidates() -> Vec<Value> {
    CANDIDATES.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

// --- Cycle-collector primitives (used by `noeta-gc`'s trial-deletion collector) ---
//
// The collector follows the heap's internal reference graph, trial-decrementing refcounts to
// discover objects kept alive only by a cycle. These expose the per-object color/buffered
// flags, raw refcount edits (no auto-free), child enumeration, and a child-preserving free.
//
// SAFETY (shared by every deref in this cluster): the collector only hands these functions
// pointer values it reached from live candidate roots this module allocated and has not freed
// (frees happen exclusively in the collector's own reclaim step, after these reads); the whole
// walk is single-threaded. Each `unsafe` below relies on exactly this invariant.

/// Read an object's cycle-collector color.
pub(crate) fn color(value: Value) -> Color {
    // SAFETY: see the cluster invariant above.
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.color
}

/// Set an object's cycle-collector color.
pub(crate) fn set_color(value: Value, color: Color) {
    // SAFETY: see the cluster invariant above.
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.color = color;
}

/// Whether the object is in the collector's candidate-root buffer.
pub(crate) fn buffered(value: Value) -> bool {
    // SAFETY: see the cluster invariant above.
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.buffered
}

/// Mark/unmark the object as buffered in the candidate-root set.
pub(crate) fn set_buffered(value: Value, buffered: bool) {
    // SAFETY: see the cluster invariant above.
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.buffered = buffered;
}

/// Raw refcount increment (no color logic). Used to restore counts during the collector's
/// scan phase.
pub(crate) fn rc_inc(value: Value) {
    // SAFETY: see the cluster invariant above.
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.refcount += 1;
}

/// Raw refcount decrement that never frees (unlike [`dec_ref`]). Used for the collector's
/// trial deletion, which restores or reclaims separately.
pub(crate) fn rc_dec(value: Value) {
    // SAFETY: see the cluster invariant above.
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.refcount -= 1;
}

/// The pointer-valued children an object references (list/map/object/enum slots). Immediates
/// are excluded — they have no heap identity and cannot participate in a cycle.
pub(crate) fn children(value: Value) -> Vec<Value> {
    // SAFETY: see the cluster invariant above.
    let obj = unsafe { &*obj_ptr(value) };
    let mut out = Vec::new();
    let mut push = |v: Value| {
        if v.is_pointer() {
            out.push(v);
        }
    };
    match &obj.payload {
        Payload::List(items)
        | Payload::Tuple(items)
        | Payload::Set(items)
        | Payload::Object { slots: items, .. }
        | Payload::Enum { data: items, .. } => items.iter().copied().for_each(&mut push),
        // Sorted-key order: `children` feeds `release_value`'s destructor walk, and destruct order is
        // observable, so it must be deterministic and match the tree-walker's sorted map. The sort is
        // GUARDED on actually holding a pointer value: an all-immediate map (e.g. every
        // Map<string, int>) has zero children, so ordering is unobservable and the O(n log n) sort
        // over n string keys is pure teardown tax — perf-profiled at ~21% of the whole 300k-entry
        // xlang assoc run before the guard.
        Payload::Map(entries) => {
            if entries.values().any(|v| v.is_pointer()) {
                let mut kv: Vec<(&MapKey, &Value)> = entries.iter().collect();
                kv.sort_unstable_by(|a, b| a.0.cmp(b.0));
                kv.into_iter().for_each(|(_, &v)| push(v));
            }
        }
        Payload::Closure { upvalues, .. } => upvalues.iter().copied().for_each(&mut push),
        Payload::Cell(inner) => push(*inner),
        // An iterator owns one reference to each source it holds.
        Payload::Iter(state) => state.children().into_iter().flatten().for_each(&mut push),
        // A future owns one reference to its thunk/step closure.
        Payload::Future(step) => push(*step),
        // A channel-send future owns one reference to the message it is queuing.
        Payload::ChannelSend(_, value, _) => push(*value),
        // A bound method handle owns one reference to its captured receiver.
        Payload::BoundMethod { recv, .. } => push(*recv),
        // A packed list holds only primitive words (no child references) — a GC leaf; a timer holds
        // only its integer deadline.
        Payload::Str(_)
        | Payload::Bytes(_)
        | Payload::Int(_)
        | Payload::NativeModule(_)
        | Payload::NativeFn(_)
        | Payload::ModuleFn { .. }
        | Payload::MethodHandle { .. }
        | Payload::PackedList { .. }
        | Payload::Timer(_)
        | Payload::Handle(..)
        | Payload::AsyncIo(_)
        | Payload::Sender(_)
        | Payload::Receiver(_)
        | Payload::ChannelRecv(_)
        | Payload::IsolateFuture(_)
        | Payload::Extern(_) => {}
    }
    // The drift guard for [`Payload::can_be_cyclic`]: anything that hands back a child value MUST
    // answer `true` there, or it would be allocated outside the registry and so escape the backup
    // mark-sweep. Checked on every traversal in debug/miri runs, which is where a newly-added node
    // variant that forgot the predicate would first appear.
    debug_assert!(
        out.is_empty() || obj.payload.can_be_cyclic(),
        "a payload with child values must answer `can_be_cyclic` — it is registered on that answer"
    );
    out
}

/// Free an object's own allocation **without** releasing its children — the cycle collector
/// frees every white object in a cycle itself, so each child is reclaimed on its own pass.
///
/// In `TrialDeletion` mode this is also the universal **deferral point**: a *buffered* object (one
/// the candidate buffer still references) is never freed here — it is painted black and left
/// allocated for the trial-deletion collector, which unbuffers it before reclaiming it. This makes
/// the deferral hold for *every* caller (the mode-aware release, and the VM's destructor-aware
/// `release_value`, which frees its last reference shallowly after running `__destruct`), so the
/// candidate buffer can never hold a dangling pointer regardless of which path reaches refcount 0.
pub(crate) fn free_shallow(value: Value) {
    if MODE.with(|m| m.get()) == CollectorMode::TrialDeletion && buffered(value) {
        set_color(value, Color::Black);
        return;
    }
    // SAFETY: the collector proved `value` is unreachable garbage and frees it exactly once;
    // children are freed by their own `free_shallow`, so they are not released here.
    let boxed = unsafe { Box::from_raw(obj_ptr(value)) };
    registry_remove(value, boxed.header.registered);
    live_dec();
    // Replace each child slot with an immediate so the `Vec`/`BTreeMap` drop does not touch
    // the (already independently freed) child objects — though dropping a `Value` is a no-op
    // regardless, this documents that ownership was surrendered.
    drop(boxed);
}

/// Read an extern-type value under a closure (extern-types X1). The caller must have checked the
/// value is an `Extern`.
pub(crate) fn with_extern<R>(
    value: Value,
    f: impl FnOnce(&dyn noeta_ext_abi::ExternValue) -> R,
) -> R {
    // SAFETY: live object this module allocated (doc contract: caller checked Extern);
    // single-threaded read.
    let obj = unsafe { &*obj_ptr(value) };
    let Payload::Extern(e) = &obj.payload else {
        panic!("with_extern on a non-extern value");
    };
    f(&**e)
}

/// Mutate an extern-type value under a closure — the receiver of a mutating method, generalizing
/// [`with_file_handle_mut`]. Single-threaded interior mutation of a heap object; the contract is
/// a GC leaf (no child `Value`s), so no retain/release bookkeeping is needed.
pub(crate) fn with_extern_mut<R>(
    value: Value,
    f: impl FnOnce(&mut dyn noeta_ext_abi::ExternValue) -> R,
) -> R {
    // SAFETY: live object this module allocated (doc contract: caller checked Extern);
    // single-threaded interior mutation with no other borrow live across `f`.
    let obj = unsafe { &mut *obj_ptr(value) };
    let Payload::Extern(e) = &mut obj.payload else {
        panic!("with_extern_mut on a non-extern value");
    };
    f(&mut **e)
}

/// Read the value held in a cell, returning a borrowed (not retained) copy. The caller must
/// have checked the value is a `Cell`.
pub(crate) fn cell_get(cell: Value) -> Value {
    // SAFETY: live object this module allocated (doc contract: caller checked Cell);
    // single-threaded read.
    let obj = unsafe { &*obj_ptr(cell) };
    let Payload::Cell(inner) = &obj.payload else {
        panic!("cell_get on a non-cell value");
    };
    *inner
}

/// Overwrite a cell's contents, retaining the new occupant and releasing the old (the cell
/// owns one reference to whatever it holds). The caller must have checked the value is a `Cell`.
pub(crate) fn cell_set(cell: Value, value: Value) {
    // SAFETY: live object this module allocated (doc contract: caller checked Cell);
    // single-threaded write, no other borrow of the cell live across the swap.
    let obj = unsafe { &mut *obj_ptr(cell) };
    let Payload::Cell(inner) = &mut obj.payload else {
        panic!("cell_set on a non-cell value");
    };
    value.inc_ref();
    let old = std::mem::replace(inner, value);
    if old.dec_ref() {
        old.free();
    }
}

/// The captured upvalue cell at `index` of a closure, returning a borrowed (not retained)
/// copy. The caller must have checked the value is a `Closure`.
pub(crate) fn closure_upvalue(closure: Value, index: usize) -> Value {
    // SAFETY: live object this module allocated (doc contract: caller checked Closure);
    // single-threaded read.
    let obj = unsafe { &*obj_ptr(closure) };
    let Payload::Closure { upvalues, .. } = &obj.payload else {
        panic!("closure_upvalue on a non-closure value");
    };
    upvalues[index]
}

/// How many upvalue cells a closure captured. The caller must have checked it is a `Closure`.
pub(crate) fn closure_upvalue_count(closure: Value) -> usize {
    // SAFETY: live object this module allocated (doc contract: caller checked Closure);
    // single-threaded read.
    let obj = unsafe { &*obj_ptr(closure) };
    let Payload::Closure { upvalues, .. } = &obj.payload else {
        panic!("closure_upvalue_count on a non-closure value");
    };
    upvalues.len()
}

/// Overwrite object slot `index` with `value`, retaining the new occupant and releasing the
/// old. This is the heap mutation primitive that lets references form cycles (and the
/// foundation for field assignment in a later slice).
pub(crate) fn set_slot(object: Value, index: usize, value: Value) {
    // SAFETY: live object this module allocated; single-threaded write, no other borrow of the
    // object live across the slot swap.
    let obj = unsafe { &mut *obj_ptr(object) };
    let Payload::Object { slots, .. } = &mut obj.payload else {
        panic!("set_slot on a non-object value");
    };
    value.inc_ref();
    let old = std::mem::replace(&mut slots[index], value);
    if old.dec_ref() {
        old.free();
    }
}

/// Overwrite object slot `index` with `value` (retaining the new occupant) and **return** the
/// displaced old value without releasing it — the caller owns it and decides its disposal (e.g. a
/// destructor-firing `release_value` rather than a plain free). The reference-count-neutral variant
/// of [`set_slot`], used by in-place struct reuse so a replaced field's `destruct` fires at the
/// right time (spec §4/§5).
pub(crate) fn replace_slot(object: Value, index: usize, value: Value) -> Value {
    // SAFETY: live object this module allocated; single-threaded write, no other borrow of the
    // object live across the slot swap.
    let obj = unsafe { &mut *obj_ptr(object) };
    let Payload::Object { slots, .. } = &mut obj.payload else {
        panic!("replace_slot on a non-object value");
    };
    value.inc_ref();
    std::mem::replace(&mut slots[index], value)
}

/// Free a shared-immutable object's own allocation (isolates I.3). Its children are freed on their
/// own [`SharedRegion`] entries, so — like the cycle collector's [`free_shallow`] — this does **not**
/// release them. A shared object was never registered with the collector, so there is no registry
/// entry to drop; only the live counter is decremented (balancing the [`alloc_shared`] at promotion).
fn free_shared(value: Value) {
    // SAFETY: `value` is a shared object this module allocated via `alloc_shared`, owned solely by the
    // region freeing it now, and freed exactly once (each region object appears once).
    let boxed = unsafe { Box::from_raw(obj_ptr(value)) };
    live_dec();
    drop(boxed);
}

/// What a payload needs from its source object to be deep-copied into a shared region: leaves clone
/// their data outright; aggregates carry their child `Value`s so [`SharedRegion::promote_value`] can
/// promote each recursively **after** the short borrow of the source object is dropped (never holding
/// a borrow across the recursive allocation). Only `Send` (value-type) payloads are representable —
/// a `!Send` payload cannot reach an isolate boundary (the checker's E0042 classifier), so promotion
/// never sees one.
enum PromoteJob {
    Leaf(Payload),
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Set(Vec<Value>),
    Map(Vec<(MapKey, Value)>),
    Object(&'static Shape, Vec<Value>),
    Enum(&'static Shape, Vec<Value>),
}

/// Whether `value`'s whole graph consists of payloads [`SharedRegion::promote`] can copy (P-PAR
/// S2): the `Send` **data** kinds. `Send`-checked (E0042) is necessary but not sufficient — a
/// function value, bound method, or channel endpoint is `Send`-shippable over `Wire` yet has no
/// promoted form, so an argument graph containing one falls back to the `Wire` copy path instead
/// of borrow-share. Immediates are trivially promotable (they pass through unchanged).
pub(crate) fn promotable_graph(value: Value) -> bool {
    if !value.is_pointer() {
        return true;
    }
    // SAFETY: a live pointer this module allocated; a short shared read (children copied out
    // before recursing, mirroring `promote_value`'s borrow discipline).
    let children: Vec<Value> = {
        let obj = unsafe { &*obj_ptr(value) };
        match &obj.payload {
            Payload::Str(_) | Payload::Bytes(_) | Payload::Extern(_) | Payload::Int(_) => {
                return true;
            }
            Payload::PackedList { .. } => return true,
            Payload::List(items) | Payload::Tuple(items) | Payload::Set(items) => items.clone(),
            Payload::Map(entries) => entries.iter().map(|(_, &v)| v).collect(),
            Payload::Object { slots, .. } => slots.clone(),
            Payload::Enum { data, .. } => data.clone(),
            _ => return false,
        }
    };
    children.iter().all(|&c| promotable_graph(c))
}

/// A **shared-immutable region** (isolates I.3): the borrow-shared heap a `concurrent { }` scope owns.
/// A value graph is [`promote`](SharedRegion::promote)d into it **once** (a single deep copy into
/// `shared`-tagged objects), then borrowed zero-copy by every isolate in the scope — `retain`/`release`
/// no-op on those objects, so no refcount is written and nothing races across threads. The scope
/// outlives every isolate (structured join), so the borrow is sound; at the join the region is
/// [`free_all`](SharedRegion::free_all)ed **wholesale**, reclaiming the whole graph at once.
///
/// This is the machinery the **real** multi-thread scheduler (I.4) uses. The deterministic sandbox
/// keeps copying per isolate (in-oracle), so neither backend constructs a region yet — it is exercised
/// by `miri` here and wired to real threads in I.4.
pub struct SharedRegion {
    /// Every object promoted into the region, each recorded exactly once (promotion dedups shared
    /// subgraphs through a memo). `free_all` frees each shallowly — children are separate entries.
    objects: Vec<Value>,
}

impl std::fmt::Debug for SharedRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRegion")
            .field("objects", &self.objects.len())
            .finish()
    }
}

impl Default for SharedRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedRegion {
    /// An empty region. The owning scope creates one at entry and `free_all`s it at the join.
    pub fn new() -> Self {
        SharedRegion {
            objects: Vec::new(),
        }
    }

    /// The number of objects currently promoted into the region (for tests/diagnostics).
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether the region holds no promoted objects.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Promote a value graph into the region: deep-copy it into fresh `shared`-tagged objects and
    /// return the new root. Immediates pass through unchanged (they carry no refcount). Shared
    /// subgraphs (a DAG — the same object reached by two paths) are copied **once** via a memo, so the
    /// promoted graph preserves the original's sharing structure. The original is left untouched and
    /// independently owned — this is a copy, not a move — so the caller's value stays local and the
    /// promoted copy alone is shared.
    ///
    /// Only `Send` value-type graphs are promotable; a `!Send` payload cannot reach here (the checker
    /// rejects a non-`Send` isolate argument with E0042), so encountering one is a bug, not input.
    pub fn promote(&mut self, root: Value) -> Value {
        let mut memo: HashMap<u64, Value> = HashMap::new();
        self.promote_value(root, &mut memo)
    }

    /// [`promote`](Self::promote) with a **caller-owned memo** (P-PAR S2): the real spawn path
    /// keeps one memo across every `isolate f(corpus)` in flight, so a corpus fanned to N workers
    /// is promoted **once** and the other N−1 spawns hit the memo. The caller owns the memo's
    /// validity: an entry keys on the *source* object's address, so the caller must keep each
    /// memoized source alive (retained) until the memo is cleared — the spawn path retains
    /// sources into the region's lifetime for exactly this reason.
    pub fn promote_with(&mut self, root: Value, memo: &mut HashMap<u64, Value>) -> Value {
        self.promote_value(root, memo)
    }

    fn promote_value(&mut self, value: Value, memo: &mut HashMap<u64, Value>) -> Value {
        if !value.is_pointer() {
            return value;
        }
        if let Some(&existing) = memo.get(&value.0) {
            return existing;
        }
        // Snapshot what the source object needs under a *short* borrow — cloning leaf data and copying
        // out child `Value`s — then drop the borrow before recursing (which allocates). A shared graph
        // is acyclic (cycles need identity + mutation, which are `class`-only, and `class` is `!Send`),
        // so promoting children before recording this node cannot loop.
        let job = {
            // SAFETY: `value` is a live pointer this module allocated; a shared read that does not
            // escape this block, and promotion (below) never mutates this object.
            let obj = unsafe { &*obj_ptr(value) };
            match &obj.payload {
                Payload::Str(s) => PromoteJob::Leaf(Payload::Str(s.clone())),
                Payload::Bytes(b) => PromoteJob::Leaf(Payload::Bytes(b.clone())),
                // An extern value is `Send` by trait bound and a leaf; `ExternBox::clone`
                // routes through `ExternValue::clone_box`.
                Payload::Extern(e) => PromoteJob::Leaf(Payload::Extern(e.clone())),
                Payload::Int(i) => PromoteJob::Leaf(Payload::Int(*i)),
                Payload::PackedList { schema, bytes } => PromoteJob::Leaf(Payload::PackedList {
                    schema,
                    bytes: bytes.clone(),
                }),
                Payload::List(items) => PromoteJob::List(items.clone()),
                Payload::Tuple(items) => PromoteJob::Tuple(items.clone()),
                Payload::Set(items) => PromoteJob::Set(items.clone()),
                Payload::Map(entries) => {
                    PromoteJob::Map(entries.iter().map(|(k, &v)| (k.clone(), v)).collect())
                }
                Payload::Object { shape, slots } => PromoteJob::Object(shape, slots.clone()),
                Payload::Enum { shape, data } => PromoteJob::Enum(shape, data.clone()),
                _ => unreachable!(
                    "a non-Send payload cannot be promoted into a shared region — the checker's \
                     E0042 Send classifier rejects a non-Send isolate argument before it reaches here"
                ),
            }
        };
        let payload = match job {
            PromoteJob::Leaf(payload) => payload,
            PromoteJob::List(items) => Payload::List(self.promote_each(&items, memo)),
            PromoteJob::Tuple(items) => Payload::Tuple(self.promote_each(&items, memo)),
            PromoteJob::Set(items) => Payload::Set(self.promote_each(&items, memo)),
            PromoteJob::Map(entries) => Payload::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, self.promote_value(v, memo)))
                    .collect(),
            ),
            PromoteJob::Object(shape, slots) => Payload::Object {
                shape,
                slots: self.promote_each(&slots, memo),
            },
            PromoteJob::Enum(shape, data) => Payload::Enum {
                shape,
                data: self.promote_each(&data, memo),
            },
        };
        let promoted = alloc_shared(payload);
        self.objects.push(promoted);
        memo.insert(value.0, promoted);
        promoted
    }

    fn promote_each(&mut self, items: &[Value], memo: &mut HashMap<u64, Value>) -> Vec<Value> {
        items
            .iter()
            .map(|&child| self.promote_value(child, memo))
            .collect()
    }

    /// Free the whole region at the scope join (isolates I.3): reclaim every promoted object at once.
    /// Consumes the region — it cannot outlive this call. Each object is freed shallowly (its children
    /// are separate region entries freed by their own iteration), so the live count returns exactly to
    /// its pre-promotion value (the leak oracle's zero-residency balance).
    pub fn free_all(self) {
        for value in self.objects {
            free_shared(value);
        }
    }
}

/// A sanctioned `Value` crossing (P-PAR S2): the root of a graph promoted into a [`SharedRegion`].
/// The blanket rule is "no `Value` crosses a thread" — this newtype is the one exception and
/// carries the safety argument: every object in the promoted graph is `shared`-tagged
/// (retain/release no-op and `is_uniquely_owned` is `false`, so no refcount is ever written and no
/// COW fast path mutates it cross-thread), it holds only `Send` data payloads (interned `&'static`
/// shape/schema handles, no `Rc`), and it is owned by a region the spawning VM frees only after
/// every borrowing worker thread has been joined.
#[derive(Debug)]
pub struct SharedRoot(u64);

// SAFETY: see the type doc — shared-tagged immutable graph, Send-only payloads, and the owning
// region outlives every borrower (the VM frees it only once no isolate is in flight, after
// joining each worker thread).
#[allow(unsafe_code)]
unsafe impl Send for SharedRoot {}

impl SharedRoot {
    /// Wrap a promoted root for shipping to a worker thread.
    pub fn new(root: Value) -> SharedRoot {
        debug_assert!(
            !root.is_pointer() || is_shared(root),
            "a SharedRoot must point into a SharedRegion"
        );
        SharedRoot(root.0)
    }

    /// The borrowed root value, usable directly on the worker thread (no rebuild, no retain —
    /// the worker's retain/release no-op on it by the shared tag).
    pub fn value(&self) -> Value {
        Value(self.0)
    }
}
