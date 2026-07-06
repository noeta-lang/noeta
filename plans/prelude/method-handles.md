# Unbound method handles — `Type.method` (prelude-redesign, slice MH)

**Status: COMPLETE — and extended beyond this plan.** MH.1/MH.2 shipped as planned; the EX.2 closing track then added ASSOCIATED handles (`ctor = Stack.new`) and BOUND handles (`f = c.bump`, receiver captured), retiring the instance-only interim and the bound-handle deferral below. Pulled into the arc (decided with user) so `map/filter/sum/len` moving to methods
does not lose the ability to pass those operations as values. Sits between P1.1 (methods added) and
the P1.2 corpus migration — `builtin_as_value.noe` migrates to method handles instead of closures.

## Why

Once `len`/`map`/… are methods, there is no bare identifier to pass as a value (`f = len` has no
referent). The language already has first-class callables (closures, builtins-as-values, P0
`ModuleFn`s); what is missing is a way to name a method **without** a receiver. This adds that.

## Design (Option A — UFCS, typed; chosen with user)

`Type.method`, when **not** immediately called, is a first-class callable value — an *unbound* method
handle that takes the receiver as its first argument:

- **Instance method** `m` of type `T`: `T.m : Fn(T, ...params) -> ret`. Calling `T.m(recv, a, b)` ≡
  `recv.m(a, b)`. Example: `list.len : Fn(list) -> int`, so `xs.map(list.len)` maps each (list)
  element through `.len()`.
- **Associated function** `f` of type `T` (`Box.new`): `T.f : Fn(params) -> ret` — the function
  itself, no receiver prepended. (Replaces today's "type member used as a value" `Unsupported`.)

`Type` may be a **user type** (methods/assoc fns in the checker's `(type, method)` table) or a
**built-in type** (`list`/`string`/`map`/`set`/`int`/`float`/`bool`/`bytes` — `Type::is_builtin_name`,
methods from the built-in method tables). Built-in type names are plain identifiers, so `list.len`
already parses as `Member { Ident("list"), "len" }`; a name shadowed by a local binding is that local,
not a type (same guard as `Box.new(...)` assoc calls today).

The **type in the syntax drives static typing**; at runtime the handle dispatches by name on the
actual receiver (instance) or as an associated call (assoc), through the existing method machinery —
so both backends agree by construction.

## Representation & execution

New callable value kind (mirrors the P0 `ModuleFn` plumbing):
- eval `Value::MethodHandle { ty: String, method: String, associated: bool }`
- VM `Payload::MethodHandle { ty, method, associated }` + `Const::MethodHandle { … }`

Dispatch when the handle is called with `args` (reuse the P2.6 `invoke` machinery / `call_method`):
- `associated == false`: `args[0]` is the receiver → `call_method(args[0], method, args[1..])`.
- `associated == true`: an associated call `ty.method(args)`.

`type_name` = "function"; renders `<fn>`; equal iff same `(ty, method, associated)` — exactly like
`ModuleFn`.

## Checker

A `Member { receiver, name }` in **value position** (not the callee of a `Call`) where `receiver` is a
type name (user type in `self.types`/`methods`, or `Type::is_builtin_name`) not shadowed by a local,
and `name` resolves to a method/assoc-fn of that type → a handle. Type it:
- instance: `Fn { params: [recv_ty, ...method_params], ret }`
- associated: `Fn { params: method_params, ret }`

Built-in receiver type: map the name to a `Type` (`list`→`List<dyn>`, `string`→`String`, …), then use
the existing `method_return`/`method_params` tables. Unknown member on a known type → E0005 (as the
qualified call already reports). Record the resolution in a `handle_sites` map (span → HandleInfo) so
both backends materialize the same handle value.

## Slices

- **MH.1 — value kind + checker typing + backends (user types).** `Box.new`, `stack.len`-style user
  handles; the `MethodHandle` value in both backends, dispatch through invoke/call_method; checker
  types `Type.method` in value position; retire the `Unsupported("type member used as a value")`.
- **MH.2 — built-in type receivers.** `list.len`, `string.upper`, etc.: resolve built-in type names as
  handle receivers, type via the built-in method tables. Then migrate `builtin_as_value.noe` to use
  handles (`xs.map(list.len)`) and add handle conformance tests.

Differential-green + leak-0 per slice. After MH, P1.2 proceeds (corpus migration to methods/handles,
then prelude removal).

**MH.2 PERFORMANCE GATE (decided with user):** factoring the VM's inline `Op::CallMethod` dispatch
into a reusable `dispatch_method` helper touches the hottest path. It MUST be behavior-neutral AND
performance-neutral: run the criterion benches + the competitor-language benchmarks (see
[[php-benchmark-perf-findings]]) before and after the refactor and confirm no negative drift on
method-dispatch-heavy workloads. Land the refactor as its own commit (differential-green, benched)
*before* adding handle dispatch on top, so a regression is attributable.

## Deferred / notes

- **Bound handles** (`x.method`, receiver already captured) are NOT in scope — only unbound
  `Type.method`. (Bound handles reintroduce the field-vs-method ambiguity; revisit if wanted.)
- Precise typing of a generic built-in handle (`list.map`) uses `List<dyn>` for the receiver element;
  good enough (dyn is top, so a concrete list is accepted).
