# P-COW — copy-on-write / unique-owner in-place list append

Status: **in progress** (eval-side slice). Source: deferred backlog row "Copy-on-write /
unique-owner in-place list append", introduced by L1 (list-building).

## The bug

`~` list concatenation copies the whole left `Vec` every time:

```rust
// crates/lang-eval/src/ops.rs — BinaryOp::Concat
(Value::List(a), Value::List(b)) => {
    let mut items = (**a).clone();        // <-- copies the entire left list
    items.extend(b.iter().cloned());
    Ok(Value::List(Rc::new(items)))
}
```

So the idiomatic accumulator loop is **O(n²)**:

```
mut acc = [];
for i in 0..n { acc ~= [i]; }   // acc = acc ~ [i]; copies acc (length i) each step → Σ i = O(n²)
```

Immutable list *semantics* are correct; only the cost is wrong. The fix preserves the semantics
and makes the loop O(n).

## Why naive `Rc::get_mut` doesn't fire — and the fix

`acc ~= [x]` desugars (in the parser) to `acc = acc ~ [x]`, i.e. a `Stmt::Binding { name: "acc",
value: Binary { Concat, Ident("acc"), [x] } }`. Evaluating the `Ident("acc")` operand **clones the
`Rc` while the scope slot still holds the original** → `strong_count == 2` → `Rc::get_mut` returns
`None` → we copy. The optimization never triggers from inside `apply_binary` alone.

**The fix is CPython's `INPLACE_ADD` trick, applied in the binding evaluator:** when the binding's
value is a self-concat `name = name ~ rhs` (also the exact shape `~=` produces), **take the old
value out of the scope slot before evaluating `rhs`**. Now the only live reference to the list is
the one we hold, so (absent other aliases) `strong_count == 1` and we mutate in place.

- **Correctness guard:** only take-out when `rhs` does **not** mention `name` (else `acc = acc ~ acc`
  would read a now-empty slot). A cheap recursive `expr_mentions(rhs, name)` gates it; on a hit we
  fall back to the ordinary path.
- **Aliasing stays correct by construction:** `b = acc; acc ~= [x]` — `b` holds another `Rc`, so
  after take-out `strong_count == 2`, `get_mut` returns `None`, we copy. `b` still sees the old list.
  This is the COW invariant: shared ⇒ copy, unique ⇒ mutate.
- **Reassignment only:** take-out applies when `name` is an existing **mutable** binding (a real
  reassignment). A fresh `acc = acc ~ …` with `acc` undefined is an error on the normal path anyway.
- **Non-list left** (`acc` is a string): `~` is display-concatenation; the take-out path detects a
  non-`List`/non-matching pair and falls back to the existing `format!`/copy behavior. Identical output.

### Observable equivalence
Result contents identical (same `extend`). No identity is observable (lists are immutable value
types). No destructor timing difference: list *elements* are cloned-in either way, and a list is not
a `__destruct`-bearing class instance, so nothing's drop is reordered. ⇒ stdout/exit unchanged ⇒
differential unaffected.

## Scope of this slice (eval / tree-walker only)

Per the sweep ordering, the **VM side is deferred to P-GC** (it needs uniqueness info from the heap
allocator). Landing eval-side alone yields a temporary perf asymmetry (eval O(n), VM O(n²)) that is
invisible to the differential. Noted in `plans/deferred.md` and the P-GC slice.

## Implementation (eval)

- `crates/lang-eval/src/lib.rs`:
  - `Scope::take(&self, name) -> Option<Value>` — remove a binding's value, leaving the slot
    absent (or a sentinel) so its `Rc` reference is released. Mirror `assign`'s mutability/parent
    search; return the displaced value + enough to restore on the fallback path.
  - In `Stmt::Binding` eval, before the generic `eval_expr(value)`, detect the self-concat shape and
    (when the guard holds + the binding is a live mutable list) run the take-out concat:
    eval `rhs`, then a `cow_concat(old, right)` helper.
  - `cow_concat(old: Value, right: Value) -> Value`: for `(List, List)` use
    `Rc::get_mut(&mut a)` → `extend` in place; else `Rc::new(copy+extend)`; non-list pair →
    display-concat (reuse existing logic). Rebind `name` to the result.
  - `expr_mentions(expr, name) -> bool` — small recursive walk; only invoked on the fast-path
    candidate (negligible cost vs. the copy it saves).
- No AST/parser/checker/VM/bytecode change. No new diagnostic code.

## Benchmark (validates the gain)

The Phase 0 parameterized accumulator bench (`acc ~= [i]` over `n ∈ {1k,2k,4k,8k}`), eval backend:
- **Before:** time ~×4 as n doubles (O(n²)).
- **After:** time ~×2 as n doubles (O(n)).

Record both columns here on completion.

| n | eval before | eval after |
|---|---|---|
| 1000 | | |
| 2000 | | |
| 4000 | | |
| 8000 | | |

## Conformance
`tests/conformance/perf/` (or `lists/`): a self-append accumulator producing the right list; the
**aliasing** case (`b = acc; acc ~= [x];` then echo both — proves `b` is unchanged); a non-list `~`
still string-concatenates; differential. (These assert *behavior* is unchanged — the perf win is in
the bench, not the conformance output.)

## Verification
- `cargo run -q -p lang-cli -- test` green; `--differential` matched / 0 skipped / agree.
- `cargo test --workspace`, clippy, fmt clean. `lang-eval` proptest path miri-clean if touched.
- Bench numbers recorded above.
