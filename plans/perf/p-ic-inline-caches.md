# P-IC — inline caches for member access & trait-method call sites

Status: **DONE** (commit pending; conformance 218 / differential 211 matched / 0 skipped / agree /
clippy + fmt clean / miri-clean). Source: deferred backlog "Inline caches for member access and
trait-method call sites (currently a hashmap/shape lookup)" (M1.4, M1.8).

**As built:** a per-run monomorphic inline cache, one slot per `LoadField`/`CallMethod` site. The
compiler assigns each such op a `cache: u32` slot id (module-global counter → `Module.cache_slots`);
the VM allocates a per-run `Vec<Option<(Rc<Shape>, u32)>>` side array (a local in `dispatch`, so it
neither borrows `self` in the loop nor leaks across runs). A hit is a raw shape-pointer compare
(`Value::object_shape_ptr`, no refcount bump); a miss resolves the slot (`slot_of`) / prototype
(`(type, method)` hashmap) and refreshes the entry, holding an `Rc<Shape>` clone so the cached
pointer key can never alias a freed shape. The cache memoizes:
- **`LoadField`** → field slot index (skips the linear `slot_of` field-name scan).
- **`CallMethod`** → method prototype (skips the hashmap lookup *and its two `String` clones* — the
  dominant cost). The `to_json`-derive special case stays ahead of the cache and only clones the
  shape name on a literal `to_json` site, off the common method-call path.

**Measured (criterion, vs the Phase 0 baseline):**
| bench | change | note |
|---|---|---|
| `vm_member_dispatch` (all n) | **−22% to −23%** (p<0.05) | the win — CallMethod IC |
| `vm/property_access` | −1.6% (p<0.05) | small; the 2-field scan was already cheap |
| `vm/dispatch_fib`, `vm/allocation_list`, `vm_accumulate` | within noise (p>0.05) | no regression from the per-run cache alloc (0 slots ⇒ no alloc) |

No observable behavior change ⇒ differential unaffected. The polymorphic-site correctness guard
(`classes/polymorphic_call_site.lang`) proves the cache refreshes on a shape miss: a field at slot 0
in one type / slot 1 in another, and a method resolving to different prototypes, accessed through one
union-typed site that alternates receiver types within a run.

## The cost

Today every `obj.field` LOAD and every trait-method/`obj.method(...)` call resolves through a
hashmap/shape lookup on each execution. In a hot loop the receiver's shape is almost always the
same from iteration to iteration (monomorphic site), so the lookup is repeated work.

## The fix (monomorphic inline cache)

Per call/property site, cache the **last seen shape → resolved slot/method**. On the next hit:
if the receiver's shape pointer matches the cached one, use the cached slot/index directly
(skip the hashmap); else fall back to the lookup and refresh the cache. This is the classic
monomorphic IC; a polymorphic (small N-entry) cache is a possible follow-up if benches show
megamorphic sites.

- **Where:** the VM only — the tree-walker has no compiled call sites to attach a cache to, and
  isn't the perf target. No cross-backend surface ⇒ no differential risk.
- **Cache key:** the `Rc<Shape>` pointer / a shape id already on the heap object.
- **Sites:** field LOAD (`property_access` bench), method dispatch (`dispatch_fib` is calls but
  not method dispatch — the Phase 0 member-dispatch bench targets this).

## Benchmark (validates the gain)
The M2.0 harness was built for exactly this: `property_access` (monomorphic field reads in a loop)
and the Phase 0 `member_dispatch` bench. Record before/after means here. Target: measurable drop on
`property_access`; the dispatch bench shows the method-call IC.

## Verification
Conformance + differential unchanged (0 skipped / agree). Workspace/clippy/fmt clean. Bench numbers
recorded. Branch `types-inferred-static`; standard trailers.

## Notes / open questions (resolve during implementation)
- Cache invalidation: shapes are immutable once created, so a cached `(shape, slot)` never goes
  stale for that shape; a different shape simply misses and refreshes. Confirm no path mutates a
  shape in place.
- Interaction with P-GC: if P-GC changes the heap object header, keep the IC key (shape id) stable
  across that change. Sequenced after P-IC, so P-GC adapts to the IC, not vice versa.
