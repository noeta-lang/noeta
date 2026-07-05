# P-SSO — one-allocation strings (small-string optimization)

**Status: in progress.** Branch `perf-map-scaffolding` (continues the tier-1 scaffolding slices).

## Why

The cross-language map-gap attribution (2026-07-05, recorded in the session findings and
`plans/perf/remaining.md`-adjacent memory) showed the hash map itself is *not* why map/string
workloads trail PHP 4–6×: raw Rust with our exact `MapStore` (hashbrown + FxHash + `format!`)
runs wordcount in 5.6 ms where PHP takes 7.8 ms — **PHP beats idiomatic native Rust** on this
shape. Zend wins with its *string representation*: a `zend_string` is one arena allocation with
the hash cached in the header. Our equivalent temp key costs **two allocations + two frees**
per iteration:

1. the `String` buffer that `BuildString` assembles, and
2. the `Box<Obj>` heap value wrapping `Payload::Str(String)`,

plus the mirrored frees when the temp is released. The tier-1 slices (itoa, `ArgBuf`,
`heap_kind` dispatch) removed the call ceremony (wordcount 33.3→28.4 ms); what remains between
us and the native floor is dominated by this round trip.

## Design

Replace the `String` inside `Payload::Str` with a **24-byte SSO string** (`compact_str`'s
`CompactString`): strings ≤ 24 bytes live inline in the payload (measured `Payload` = 80 bytes,
so no size change — the interpolated keys these workloads build all fit), longer ones spill to
a heap buffer with capacity (amortized `push_str` growth preserved, so `ConcatInPlace` — the
strcat win — keeps its O(n) contract). A short-lived string then costs **one** allocation (the
`Obj`), like PHP.

Map keys get the same treatment: `MapStore = HashMap<CompactString, Value, Fx>`.
`CompactString: Borrow<str> + Hash`-consistent-with-`str`, so `&str` probes keep working;
moving an inline key into the map is a 24-byte copy, no allocation (`take_string_in_place`
stays a move).

Deliberately **not** in scope (measured, not worth it here):

- **Hash caching in the header** — PHP's other trick. Our keys are short (FxHash of 8 bytes
  ≈ single-digit ns); caching pays on long keys only. Revisit if a workload shows it.
- **Interning** — a dedup table pays a probe on every string *build*; only wins when the same
  text recurs as a heap value many times (wordcount's 500 keys × 400 rebuilds would win, but
  the SSO inline representation already makes those builds allocation-free, which is strictly
  better than winning the dedup).
- The live-object registry (measured: a wash) and map pre-sizing (measured: a regression).

## Slices

- **S1** — `Payload::Str(String)` → `Payload::Str(CompactString)`, behavior-neutral: every
  accessor keeps its signature (`as_string()` still clones out an owned `String`; boundary
  conversions at the cold public API). `BuildString` assembles a `CompactString` directly
  (`display_into` takes the new buffer type) so a short interpolation never touches the
  allocator. Gate: full suite + differential byte-identical + miri + leak/jit oracles;
  `vm_interp/*`, strcat unchanged.
- **S2** — `MapStore` keys → `CompactString`; `map_insert`/`map_remove`/`take_string_in_place`
  move the SSO value; public builders/accessors (`Value::map(BTreeMap<String, _>)`,
  `map_entries()`) convert at the boundary. Gate: as S1, plus `vm_map_*` criterion and the
  wordcount/assoc wall-clock gauges.
- **S3 (measure first)** — single-probe RMW write via hashbrown `raw_entry_mut` with a
  precomputed hash: occupied hit rewrites the value without materializing a key at all
  (wordcount writes 200k times into 500 slots — today every write allocates a key `String`
  that `insert` immediately drops). Only if S2's numbers still show the double-probe cost.

## Targets

Wordcount 28.4 ms → below ~20 ms (native floor 5.6, PHP 7.8); assoc 34 ms → toward the 25s.
Strcat and the JIT loop/call benchmarks must not regress (strcat is currently a full-field win).
