//! The **iterator engine** (Tracks I.1a–I.2): lazy iterator values and their adapters
//! (`take`/`drop`/`chain`/`enumerate`/`zip`/`map`/`filter`/generators), and the pull machinery
//! (`iter_next_apply` and the drains built on it). `impl Value` methods moved verbatim from the
//! crate root (audit-1 finding 8) — same crate, so private access is preserved; no behavior
//! change (the documented short-borrow discipline in `iter_next_apply` is untouched).

use crate::heap::{self, IterShape, IterState, Payload};
use crate::{IterAbort, Value};

impl Value {
    /// A lazy iterator value (Track I.1a) cursoring over `list` from the start. The iterator owns one
    /// reference to its backing list (retained here); the caller's reference to `list` is untouched.
    pub fn iter(list: Value) -> Value {
        list.inc_ref();
        heap::alloc(Payload::Iter(IterState::List { list, cursor: 0 }))
    }

    /// A `take(n)` adapter: yields at most `n` elements from `source` (Track I.1b). The adapter owns
    /// one reference to `source` (retained here); the caller's reference to `source` is untouched.
    pub fn iter_take(source: Value, n: usize) -> Value {
        source.inc_ref();
        heap::alloc(Payload::Iter(IterState::Take {
            source,
            remaining: n,
        }))
    }

    /// A `drop(n)` adapter: skips the first `n` elements of `source`, yields the rest (Track I.1b).
    pub fn iter_drop(source: Value, n: usize) -> Value {
        source.inc_ref();
        heap::alloc(Payload::Iter(IterState::Drop { source, pending: n }))
    }

    /// A `chain(other)` adapter: yields all of `first`, then all of `second` (Track I.1b). Owns one
    /// reference to each.
    pub fn iter_chain(first: Value, second: Value) -> Value {
        first.inc_ref();
        second.inc_ref();
        heap::alloc(Payload::Iter(IterState::Chain { first, second }))
    }

    /// An `enumerate()` adapter: yields `(index, element)` tuples from `source`, indexing from 0
    /// (Track I.1b.2). Owns one reference to `source`.
    pub fn iter_enumerate(source: Value) -> Value {
        source.inc_ref();
        heap::alloc(Payload::Iter(IterState::Enumerate { source, index: 0 }))
    }

    /// A `zip(other)` adapter: yields `(a_elem, b_elem)` tuples, stopping at the shorter source
    /// (Track I.1b.2). Owns one reference to each source.
    pub fn iter_zip(a: Value, b: Value) -> Value {
        a.inc_ref();
        b.inc_ref();
        heap::alloc(Payload::Iter(IterState::Zip { a, b }))
    }

    /// A `map(f)` adapter: yields `func(element)` for each element of `source` (Track I.1c). Owns one
    /// reference to `source` and one to the closure `func`.
    pub fn iter_map(source: Value, func: Value) -> Value {
        source.inc_ref();
        func.inc_ref();
        heap::alloc(Payload::Iter(IterState::Map { source, func }))
    }

    /// A `filter(f)` adapter: yields the elements of `source` for which `pred(element)` is true
    /// (Track I.1c). Owns one reference to `source` and one to the closure `pred`.
    pub fn iter_filter(source: Value, pred: Value) -> Value {
        source.inc_ref();
        pred.inc_ref();
        heap::alloc(Payload::Iter(IterState::Filter { source, pred }))
    }

    /// A generator iterator (Track G): `step` is a closure (a state machine over `mut`-captured cells)
    /// invoked once per `next()` and returning `?T`. Owns one reference to the closure.
    pub fn iter_gen(step: Value) -> Value {
        step.inc_ref();
        heap::alloc(Payload::Iter(IterState::Gen { step }))
    }

    /// Whether this is an iterator.
    pub fn is_iter(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Iter(_)))
    }

    /// If this iterator is directly backed by a list (`xs.iter()` with no adapters), return the
    /// backing list value and the cursor's current position, and **drain** it (advance the cursor to
    /// the end). Used by the eager reduction delegation (packed-reductions arc) so `xs.iter().sum()`
    /// folds the same buffer as `xs.sum()` and thus width-wraps identically. The returned list value
    /// is borrowed (still owned by the iterator), so the caller must not release it. `None` for an
    /// adapter/generator iterator or a non-iterator.
    pub fn iter_drain_list(self) -> Option<(Value, usize)> {
        heap::with_payload_mut(self, |p| {
            let Payload::Iter(IterState::List { list, cursor }) = p else {
                return None;
            };
            let start = *cursor;
            *cursor = list.list_len().unwrap_or(start);
            Some((*list, start))
        })
    }

    /// Advance the iterator, returning the next element — a freshly-retained owning reference the
    /// caller takes ownership of — or `None` at end. The caller must have checked [`Value::is_iter`].
    ///
    /// `apply(func, arg)` runs a `map`/`filter` closure on an element (consuming `arg`'s reference,
    /// returning an owned result), letting the closure adapters call back into the backend's call
    /// machinery (Track I.1c); a closure-free pipeline never reaches it. The generic `E` is the
    /// backend's own call-error type, surfaced through [`IterAbort::Closure`].
    ///
    /// **Borrow discipline (soundness):** an adapter reads its [`IterShape`] under a *short* borrow,
    /// then recurses into its source / runs the closure with **no** borrow held on this node, and
    /// finally writes any cursor change under another short borrow. So even if the user closure
    /// re-enters this same iterator, no live `&mut` to the node is aliased (miri-verified). Each
    /// source is also a distinct allocation (an iterator can never be its own source).
    pub fn iter_next_apply<E>(
        self,
        apply: &mut dyn FnMut(Value, Value) -> Result<Value, E>,
    ) -> Result<Option<Value>, IterAbort<E>> {
        loop {
            let shape = heap::with_payload(self, |p| match p {
                Payload::Iter(state) => Some(state.shape()),
                _ => None,
            });
            let Some(shape) = shape else {
                return Ok(None);
            };
            match shape {
                // No recursion and no user code: the cursor is read and advanced under one short
                // borrow. `list_get` shares the list's reference; retain it for the new owner.
                IterShape::List => {
                    return Ok(heap::with_payload_mut(self, |p| {
                        let Payload::Iter(IterState::List { list, cursor }) = p else {
                            return None;
                        };
                        let e = list.list_get(*cursor)?;
                        *cursor += 1;
                        e.inc_ref();
                        Some(e)
                    }));
                }
                IterShape::Take { source, remaining } => {
                    if remaining == 0 {
                        return Ok(None);
                    }
                    return Ok(match source.iter_next_apply(apply)? {
                        Some(e) => {
                            heap::with_payload_mut(self, |p| {
                                if let Payload::Iter(IterState::Take { remaining, .. }) = p {
                                    *remaining -= 1;
                                }
                            });
                            Some(e)
                        }
                        None => None,
                    });
                }
                IterShape::Drop { source, pending } => {
                    if pending > 0 {
                        match source.iter_next_apply(apply)? {
                            Some(skipped) => {
                                skipped.release(); // the skipped element's retained reference
                                heap::with_payload_mut(self, |p| {
                                    if let Payload::Iter(IterState::Drop { pending, .. }) = p {
                                        *pending -= 1;
                                    }
                                });
                                continue; // skip the next pending element
                            }
                            None => {
                                heap::with_payload_mut(self, |p| {
                                    if let Payload::Iter(IterState::Drop { pending, .. }) = p {
                                        *pending = 0;
                                    }
                                });
                                return Ok(None);
                            }
                        }
                    }
                    return source.iter_next_apply(apply);
                }
                IterShape::Chain { first, second } => {
                    if let Some(e) = first.iter_next_apply(apply)? {
                        return Ok(Some(e));
                    }
                    return second.iter_next_apply(apply);
                }
                // The source's element (already retained) and the immediate index are handed to the
                // new tuple, which takes ownership of one reference to each.
                IterShape::Enumerate { source, index } => {
                    return Ok(match source.iter_next_apply(apply)? {
                        Some(e) => {
                            let tuple = Value::tuple(vec![Value::int(index as i64), e]);
                            heap::with_payload_mut(self, |p| {
                                if let Payload::Iter(IterState::Enumerate { index, .. }) = p {
                                    *index += 1;
                                }
                            });
                            Some(tuple)
                        }
                        None => None,
                    });
                }
                // Pull from both, shorter wins. If `a` ran dry there is nothing to release; if only
                // `b` did, release `a`'s already-retained element so it does not leak.
                IterShape::Zip { a, b } => {
                    let Some(ea) = a.iter_next_apply(apply)? else {
                        return Ok(None);
                    };
                    return Ok(match b.iter_next_apply(apply)? {
                        Some(eb) => Some(Value::tuple(vec![ea, eb])),
                        None => {
                            ea.release();
                            None
                        }
                    });
                }
                // `apply` consumes the source element's reference and returns the mapped result (owned).
                // On a closure error the call already consumed that reference, so nothing leaks here.
                IterShape::Map { source, func } => {
                    let Some(e) = source.iter_next_apply(apply)? else {
                        return Ok(None);
                    };
                    return apply(func, e).map(Some).map_err(IterAbort::Closure);
                }
                // Retain the element once for the predicate call (which consumes a reference) and keep
                // one to hand back if it passes. On a closure error release the held reference; a
                // non-bool verdict is a typed abort the backend phrases as a diagnostic.
                IterShape::Filter { source, pred } => {
                    let Some(e) = source.iter_next_apply(apply)? else {
                        return Ok(None);
                    };
                    e.inc_ref();
                    let verdict = match apply(pred, e) {
                        Ok(v) => v,
                        Err(err) => {
                            e.release();
                            return Err(IterAbort::Closure(err));
                        }
                    };
                    match verdict.as_bool() {
                        Some(true) => {
                            verdict.release();
                            return Ok(Some(e));
                        }
                        Some(false) => {
                            verdict.release();
                            e.release();
                            continue; // try the next source element
                        }
                        None => {
                            let name = verdict.type_name();
                            verdict.release();
                            e.release();
                            return Err(IterAbort::FilterNotBool(name));
                        }
                    }
                }
                // A generator (Track G): run the step closure (one resume arg, here unit) and
                // interpret its returned `?T`. `option_take` consumes the returned Option wrapper.
                IterShape::Gen { step } => {
                    let opt = match apply(step, Value::unit()) {
                        Ok(v) => v,
                        Err(err) => return Err(IterAbort::Closure(err)),
                    };
                    return Ok(opt.option_take());
                }
            }
        }
    }

    /// Deconstruct an `Option` value a generator step returned: `some(x)` → `Some(x)` (the payload
    /// retained for the new owner), `none`/anything else → `None`. Consumes one reference to `self`
    /// (the Option wrapper).
    fn option_take(self) -> Option<Value> {
        if !self.is_pointer() {
            return None;
        }
        let extracted = heap::with_payload(self, |p| match p {
            Payload::Enum { shape, data } if shape.variant.as_deref() == Some("some") => {
                data.first().copied()
            }
            _ => None,
        });
        if let Some(x) = extracted {
            x.inc_ref(); // retain for the new owner before the wrapper is released
        }
        self.release(); // drop the Option wrapper (its `some` payload now survives via the bump above)
        extracted
    }

    /// Advance a **closure-free** iterator (no `map`/`filter` in the pipeline). The caller must have
    /// checked [`Value::is_iter`]; reaching a closure adapter without an applier panics. Used by the
    /// closure-free terminals below and the unit tests.
    pub fn iter_next(self) -> Option<Value> {
        let mut applier = |_: Value, _: Value| -> Result<Value, ()> {
            unreachable!("closure-free iterator reached a map/filter adapter without an applier")
        };
        match self.iter_next_apply(&mut applier) {
            Ok(v) => v,
            Err(_) => unreachable!("closure-free pipeline cannot abort"),
        }
    }

    /// Drain a closure-free iterator from its current cursor into a new list — each element retained
    /// into it. The caller must have checked [`Value::is_iter`].
    pub fn iter_collect(self) -> Value {
        let mut out = Vec::new();
        while let Some(e) = self.iter_next() {
            out.push(e);
        }
        Value::list(out)
    }

    /// Drain a closure-free iterator, summing its numeric elements (Track I.1b.2) — `int` if every
    /// element is an `int`, else `float`. Mirrors the eager `sum` builtin's accumulation exactly so
    /// the two paths agree. Each drained element's retained reference is released; on the first
    /// non-numeric element it is dropped and its type name returned as `Err` for the caller's
    /// diagnostic. The caller must have checked [`Value::is_iter`].
    pub fn iter_sum(self) -> Result<Value, &'static str> {
        let mut int_total: i64 = 0;
        let mut float_total: f64 = 0.0;
        let mut any_float = false;
        while let Some(e) = self.iter_next() {
            if let Some(i) = e.as_int() {
                int_total = int_total.wrapping_add(i);
            } else if let Some(f) = e.as_float() {
                any_float = true;
                float_total += f;
            } else {
                let name = e.type_name();
                e.release();
                return Err(name);
            }
            e.release();
        }
        Ok(if any_float {
            Value::float(float_total + int_total as f64)
        } else {
            Value::int(int_total)
        })
    }
}
