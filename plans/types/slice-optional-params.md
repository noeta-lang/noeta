# Slice (mid-track feature): Optional parameters & default values

Status: **done** (conformance 157 / differential 151 matched, 0 skipped, backends agree). Closures and enum-variant fields deliberately excluded (the parser does not accept `=` there); supporting closure defaults is a possible follow-up but not currently needed.

A language feature requested mid-track (like `while`, ranges, and `break`/`continue` before it): a named function/method parameter may carry a default value, `fn greet(name: string, greeting: string = "Hello"): string`, and a call may omit the trailing defaulted arguments. This is **not** part of the inferred-static S-numbering; it is folded in because the user asked for it after S4.

## Decided semantics

- **Syntax:** `name: T = expr` in a parameter list. The type annotation is still required (named signatures are mandatory under inferred-static — `E0022`); the default is checked against it.
- **Trailing-only:** once a parameter has a default, every following parameter must also have one. A required parameter after an optional one is a compile error — new code **`E0026 RequiredAfterOptional`**.
- **Allowed on:** free functions, associated functions, and instance methods (all "named" callables). **Forbidden on** anonymous closures (`fn(x) =>`) and enum-variant fields — the parser does not accept `=` there.
- **Default scope = globals only.** A default expression is checked and evaluated in a scope that sees **only module-level names** — not other parameters, not `self`, not fields. A reference to a parameter is therefore an ordinary `E0005 UnknownName`. This is what lets the two backends evaluate defaults identically: the tree-walker evaluates them in the closure's captured (global) scope, and the VM evaluates them as a globals-only zero-arg **thunk** — both reach the same globals, so the differential holds.
- **Re-evaluated per call.** Each call that omits an argument re-runs the default expression (no shared-mutable-default footgun à la Python).
- **Indirect calls** (a function value called through a binding, `f = greet; f("x")`) do **not** apply defaults — both backends require the full arity there, so they still agree. Defaults are a property of named call resolution, and the mechanism that fills them lives on the callee chunk/closure, which an indirect call still reaches; so in practice an indirect call *does* fill (the fill is callee-side). The conformance corpus exercises only direct/method/associated calls.

## Mechanism (callee-side fill — each backend authoritative for its own call resolution)

Chosen over caller-side substitution so neither backend has to duplicate the other's call-target resolution (which would risk differential divergence). The default values are filled in **by the callee** when fewer arguments arrive than parameters exist.

- **Tree-walker (`lang-eval`):** `Closure` carries `defaults: Vec<Option<Expr>>` parallel to `params`. `call_closure` / `call_method_on` accept an arity in `[required, total]`; for each missing trailing parameter they evaluate its default in a child of the closure's `captured` (global) scope and bind it.
- **VM (`lang-vm` / `lang-compiler` / `lang-bytecode`):** each default expression compiles to a **globals-only zero-parameter thunk proto**; the enclosing `Chunk` gains `defaults: Box<[(reg, thunk_proto)]>` mapping a parameter register to the thunk that fills it. `Op::Call`, `Op::CallMethod`, and `call_value` accept the arity range and, for each defaulted register `>=` the supplied count, run the thunk (`self.run` on a fresh single-frame stack, like `map`/`filter` callbacks) and place the result.

Both paths reach only globals, so a default referencing a global constant or function behaves identically; a default referencing a parameter is rejected statically before either backend runs.

## Diagnostics

- New: **`E0026 RequiredAfterOptional`** — a required parameter follows one with a default value.
- Reused: `E0007 TypeMismatch` (default value's type does not match the parameter type; also an over-arity call), `E0005 UnknownName` (a default references a non-global, e.g. another parameter), `E0022 MissingSignature` (a defaulted parameter still needs its type).
- Append-only note: this consumes **E0026**; the next free code becomes **E0027** (S5 trait coherence is renumbered from its previously-reserved E0026 to E0027).

## Oracle posture

The checker is shared, so all new static rejections are identical on both backends. The fill mechanism is implemented natively in each backend (no new `Unsupported` surface), so `--differential` stays at **0 skipped**. New conformance cases cover: a free function, an associated function, and an instance method each omitting a default; trailing-only violation (`E0026`); default type mismatch (`E0007`); a default that references a parameter (`E0005`); and over-/under-arity calls.

## Verification (before commit)

- `cargo run -q -p lang-cli -- test --differential` → matched / 0 skipped / backends agree.
- `cargo run -q -p lang-cli -- test` → full conformance green (count grows).
- `cargo test` workspace-wide (new parser/checker/eval/vm unit tests; disassembly snapshots updated).
- `cargo clippy --all-targets` + `cargo fmt --all --check`.
