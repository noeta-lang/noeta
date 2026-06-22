# Slice F2 — prelude collection builtins as first-class values

Status: done

Closes the "reference to a prelude value/builtin as a value" row of `plans/deferred.md`'s VM-completeness section, for the four collection builtins. The tree-walker already represents builtins as ordinary first-class values (`Value::Builtin`, declared as globals); the VM special-cased them only at *direct*-call sites (`Op::CallBuiltin`), so a bare reference (`f = len; f(xs)`, or `map(xs, len)`) compiled to `Unsupported` and was skipped.

## Goal

`len`, `map`, `filter`, and `sum` can be stored in a binding, passed as arguments, and called indirectly — byte-identical on both backends.

## Scope

- In: `len`/`map`/`filter`/`sum` as first-class values (`Payload::NativeFn(Builtin)`), dispatched at runtime from `Op::Call` and from `call_value` (so `map(xs, len)` works); `len` on a user object via an indirect call re-enters its `Length` method.
- Out (kept `Unsupported`, narrowed registry row): the constructors `Ok`/`Err`/`some`, `panic`, and `next_id` as first-class *values* (exotic; they need hand-matched runtime arity/error text). `none` already works (it is a value, not a function).

## Design

The four builtins already share one identifier (`lang_bytecode::Builtin`) and one runtime helper (`Vm::call_builtin(Builtin, &[Value], span)`), which is already differential-matched via `Op::CallBuiltin`. So:

- lang-value stores `Payload::NativeFn(Builtin)` (a GC leaf like `NativeModule`); lang-value gains a `lang-bytecode` dep for the `Builtin` type (no cycle — lang-bytecode does not depend on lang-value).
- A bare prelude reference compiles to `Op::LoadNativeFn { dst, func }` (only for names `Builtin::from_name` recognizes; `none` still lowers to its enum; anything else stays `Unsupported`).
- The VM's `Op::Call` and `call_value` dispatch a `NativeFn` through `call_native_fn`, which reuses `call_builtin` — except `len` on a user object, which re-enters the object's `len` method (mirroring the `Op::CallBuiltin` object case), via the same `self.run`-reentry `call_value` uses.

Reusing `call_builtin` means the indirect path inherits the exact arity/error text the direct path is already checked against — no new divergence surface.

## Definition of done — met

`closures/builtin_as_value` (builtin stored + called, `map(xs, len)`, `filter` stored) and `closures/builtin_value_on_object` (`len`-as-value re-entering an object's `Length` method) run identically on both backends. Differential 102 → 104 matched / **0 skipped**; 110 conformance; lang-value unit test for the native-fn value + equality; miri green; fmt/clippy clean. The registry row is struck for `len`/`map`/`filter`/`sum`; the constructor/`panic`/`next_id`-as-value tail is recorded as still open.

## Outcome notes

- Reused `lang_bytecode::Builtin` as the value's id (`Payload::NativeFn(Builtin)`) — lang-value gained a lang-bytecode dep (no cycle; lang-bytecode doesn't depend on lang-value). One source of truth for the builtin set.
- The indirect path reuses `call_builtin`, so it inherits the exact arity/error text the direct `CallBuiltin` path is already differential-checked against — no new divergence surface. The only bespoke bit is `len`-on-an-object, which re-enters the `Length` method via the same `self.run` reentry `call_value` uses for closures.
- Dispatch is wired into both `Op::Call` (register-borrowed args) and `call_value` (owned args, released after) so a builtin works both as a stored callee and as a function argument to `map`/`filter`.
