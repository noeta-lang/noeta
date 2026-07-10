# Packed keys — `@packed` structs as Map keys and Set elements

*Status: **S0–S4 COMPLETE** (2026-07-10, branch `packed-keys`). Tag: P-PKEY.*

## Motivation

The packed arc's domain is spatial/data-oriented work (vec/quat, kernels, SoA columns), whose
bread-and-butter collection idioms are the spatial hash (`Map<Cell, List<int>>`) and the visited
set (`Set<Cell>`). Neither has a path today:

- `Map<Cell, int>` **type-checks, then aborts at runtime** — E0007 "`object` cannot key a map".
  (A checker/runtime divergence in itself: `noeta check` passes a program that deterministically
  aborts.)
- `#{Cell{…}}` / `xs.to_set()` reject anything but int/float/string ("single orderable type").

The only workaround is string-encoding keys (`"${x},${y}"`) — lossy, untyped, and reconstructing
the coordinates from `keys()` means parsing strings back apart. (The plan originally also claimed
a performance win over string keys; the S3 bench refuted that for short keys — see the bench
outcome at the bottom.)

`@packed` is the right capability marker: it already means *value type, fixed width, content
identity, no interior aliasing* — exactly what a key must be.

## Design

### Key capability

A packed struct is **key-capable** iff every field is `int`, a fixed-width `{i,u}N`, `bool`, or a
nested key-capable packed struct. **Float/f32 fields are excluded** (NaN ≠ NaN and `-0.0 == 0.0`
make float keys a footgun; a deliberate bit-pattern opt-in can come later). The checker already
computes exactly this shape (`packed_layout`, `crates/noeta-check/src/packed.rs:219`); the arc
narrows it (no floats) into a `key_capable_packed` predicate.

### Identity and order: the content snapshot

*(As built in S1: the design below was superseded by a structured **field-values snapshot** —
`PackedKeyField::{Int, Bool, Struct}` in declaration order — because `map.keys()` must
reconstruct full struct values from keys, which raw bytes cannot do without a schema. The
derived field-wise `Ord` gives the same semantic order the memcomparable bytes would have; the
key also carries the value's display form for render/JSON, excluded from identity.)*

A packed key's identity is **(qualified type name, canonical field bytes)** — name-based like the
namespaced-types arc (backend-neutral: the eval reference has no `&'static Shape`), bytes in the
classic *memcomparable* form so one encoding serves all three contracts:

- each int field: 8 bytes **big-endian with the sign bit flipped** (order-preserving);
- each bool: one byte 0/1; nested packed structs recurse in declaration order.

Then `Hash` = hash(name, bytes), `Eq` = (name, bytes) equality, and `Ord` = name, then **byte-wise
lexicographic — which equals field-wise semantic order** by construction. Map iteration order
(observable: display, JSON, `map_keys` all sort by `MapKey::Ord`) therefore sorts packed keys by
field values, not representation accidents. Both backends encode from field values in declaration
order, so the differential pins byte-identical keys.

### Runtime shape knowledge

A standalone packed value is an ordinary `Payload::Object { shape, slots }`, and `Shape` today
carries **no packedness** — the runtime cannot tell a key-capable value from any other struct.
S0 adds `key_capable: bool` to `noeta-object::Shape`, threaded from the compiler (which has the
checker's knowledge) through the bytecode module's shape table to the VM's interned shapes — and
the equivalent marker on the eval backend's struct values. Key *encoding* then reads field values
straight from the slots (no `PackedSchema` byte layout needed).

### The four surfaces

1. **Maps** — a third `MapKey` variant in `noeta-native::map_key`:
   `Packed { type_name: CompactString, bytes: Box<[u8]> }`, following the `Extern` precedent
   (Hash/Eq/Ord in one place, both backends share it by construction). Conversion sites:
   VM `Op::RequireMapKey`/`Op::MakeMap`/`methods.rs` key extraction; eval `value_map_key`.
   `render()` shows the struct display form (JSON keys = display string, lossy — the extern
   precedent).
2. **Sets** — sets are a canonical sorted `Vec<Value>` ordered by `compare_primitive`, no key
   abstraction at all. Extend the comparator (both backends): two objects whose shapes are
   key-capable compare by (type name, then field-wise slot order — matching the memcomparable
   order exactly). `canonical_set` then accepts them; `#{…}` literals and `to_set` follow for
   free (the set literal desugars to `to_set` in the parser).
3. **Checker** — the two extern-only key gates (`Map<K,_>`/`Set<T>` formation at
   `noeta-check/src/lib.rs:3010-3037`, map literals at `:4004-4016`) learn: *accept* a
   key-capable packed struct, *reject* any other named/record type **at check time** (closing
   the divergence — today's silent pass-then-abort). Reuses `TypeMismatch` like the extern gates
   (no new code unless review says otherwise; E0050 is next free if wanted).
4. **Runtime backstop** — `RequireMapKey`/`value_map_key`/`canonical_set` keep their errors for
   what the checker can't see (`dyn`), with the message extended to name packed structs as
   key-capable.

### Deliberately out of scope

- **Float/f32 key fields** (bit-pattern keys) — separate opt-in decision.
- **Flat/SoA storage inside Map/Set** — no workload evidence; the map residual is per-op
  scaffolding, not value locality. Revisit if the profiler says otherwise.
- **Isolate Wire crossing** — non-string-keyed maps are already gated at the boundary (E0042,
  the extern-key precedent); packed keys inherit that. Widening `Wire::Map` is future work.
- **`WidthIntMethod`-style intrinsics on keys** — nothing to do here.

## Slices

| Slice | Contents | Gate |
|---|---|---|
| **S0** | `Shape.key_capable` plumbing end to end (checker predicate → compiler → bytecode shape table → VM interned shapes; eval-side marker). No behavior change. | differential + full suites green, disasm snapshots unchanged |
| **S1** | Map keys: `MapKey::Packed`, memcomparable encoder from slots (both backends), conversion sites, render/JSON, checker *acceptance* for `Map<K,_>` formation + literals. | differential, leak 0, corpus (`packed/packed_map_keys.noe`), map-iteration order pinned |
| **S2** | Set elements: comparator extension (both backends), `canonical_set`, `#{…}`/`to_set`, checker acceptance for `Set<T>`. | differential, corpus (`packed/packed_set_elements.noe`) |
| **S3** | Check-time *rejection* of non-key-capable named types in key/element position (divergence fix), runtime message updates, docs (Collections + packed docs), spatial-hash bench (`Map<Cell,int>` vs string-encoded keys). | full suites, bench recorded here |

Standing gates per slice: eval↔VM differential (0 divergence), leak oracle residency 0,
`--jit-differential` (keys are heap ops — the JIT bails on them; must stay byte-identical),
clippy + fmt, commit per green slice.

## Bench outcome (S3) — measured, honest: the perf claim did NOT hold

Counting-grid workload (200k iterations of `grid[c] = grid.get_or(c, 0) + 1`, release, pinned,
per-sample-alternating min-of-13):

| keys | time |
|---|---|
| `"${x},${y}"` string | **37.1 ms** |
| `Map<Cell, int>` packed | 58.5 ms (~1.6× slower) |

**Why the plan's premise was stale:** it assumed string keys pay "interpolation + string hash +
allocation per probe" — but P-SSO already made short string keys **allocation-free** (≤24-byte
inline `CompactString`, and P-VMT's single-pass interpolation), so the tiny-coordinate idiom costs
the string path ~zero heap traffic. The packed path pays ~3 allocations per iteration: the fresh
`Cell` object (`MakeStruct`) plus one `Vec<PackedKeyField>`+`Box` per key build (probe and set).
Two S3 reworks already landed to get from 3.1× to 1.6×: display is **no longer carried in keys**
(a `packed_names` registry — type name → field names, registered once per type at load/declare —
derives display on demand; key builds format nothing), and the field walk borrows slots in place.

**What packed keys deliver today:** the capability (typed spatial idiom, content identity,
`keys()` returning real values, deterministic field-wise order) — not a speed win on short keys.
They should win where string keys exceed SSO (nested/long keys) or where key *construction*
dominates less.

**S4 (int keys, follow-on landed):** `MapKey::Int` — `int` + the erased `{i,u}N` family key
maps; `float`/`f32`/`bool` and user records/enums are rejected statically (the checker's
key-capability now covers every named type). An int key is an immediate: zero-allocation build,
one-word hash. Measured on a 200k ID-counting map (release, pinned, alternating min-of-13):
int keys **39.7 ms** vs string keys 45.8 ms — **1.15× faster** with identical output (the win is
bounded by the shared loop/arithmetic cost per iteration; the map-op portion shrinks much more).
A spatial grid can also pack `(x, y)` into one int key manually to ride this path.

**Follow-up path if the perf matters** (not started): a zero-alloc probe adapter
(`Equivalent<MapKey>` + a canonical hash walked directly off the object's slots — kills the
key-build on `get_or`/`has`/index), and reuse-analysis recycling of the per-iteration key temp
(`MakeStruct` consumed immediately). Estimated to close most of the gap; parity with SSO strings
on 2-int keys is not guaranteed.
