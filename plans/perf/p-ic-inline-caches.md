# P-IC — inline caches for member access & trait-method call sites

Status: **planned** (sweep item #2). Source: deferred backlog "Inline caches for member access and
trait-method call sites (currently a hashmap/shape lookup)" (M1.4, M1.8).

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
