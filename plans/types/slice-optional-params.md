# Slice (mid-track feature): Optional parameters & default values

Status: **done** (conformance 160 / differential 154 matched, 0 skipped, backends agree). Supported on free functions, associated functions, methods, **and closures** (capture-aware — see the closures section below). Only enum-variant fields are excluded (the parser does not accept `=` there).

A language feature requested mid-track (like `while`, ranges, and `break`/`continue` before it): a named function/method parameter may carry a default value, `fn greet(name: string, greeting: string = "Hello"): string`, and a call may omit the trailing defaulted arguments. This is **not** part of the inferred-static S-numbering; it is folded in because the user asked for it after S4.

## Decided semantics

- **Syntax:** `name: T = expr` in a parameter list. The type annotation is still required (named signatures are mandatory under inferred-static — `E0022`); the default is checked against it.
- **Trailing-only:** once a parameter has a default, every following parameter must also have one. A required parameter after an optional one is a compile error — new code **`E0026 RequiredAfterOptional`**.
- **Allowed on:** free functions, associated functions, instance methods, and closures. **Forbidden on** enum-variant fields — the parser does not accept `=` there.
- **Default scope = the function's definition scope.** A default is checked and evaluated where the function is *defined*, not where it is called, and it does **not** see the function's own parameters. For a top-level function or method that scope is the module's globals; for a closure it is the captured (enclosing) scope, so a closure default may reference a captured variable — exactly like the closure body. A default that reaches for a *sibling parameter* resolves to nothing — a runtime `E0005 UnknownName`, as elsewhere in the language. This single rule keeps the two backends identical: the tree-walker evaluates a default in the closure's `captured` scope; the VM evaluates it as a zero-arg **thunk** compiled with the function's own upvalue layout and handed the closure's captured cells at call time, so both reach the same bindings.
- **Re-evaluated per call.** Each call that omits an argument re-runs the default expression (no shared-mutable-default footgun à la Python).
- **Indirect calls** (a function value called through a binding, `f = greet; f("x")`) do **not** apply defaults — both backends require the full arity there, so they still agree. Defaults are a property of named call resolution, and the mechanism that fills them lives on the callee chunk/closure, which an indirect call still reaches; so in practice an indirect call *does* fill (the fill is callee-side). The conformance corpus exercises only direct/method/associated calls.

## Mechanism (callee-side fill — each backend authoritative for its own call resolution)

Chosen over caller-side substitution so neither backend has to duplicate the other's call-target resolution (which would risk differential divergence). The default values are filled in **by the callee** when fewer arguments arrive than parameters exist.

- **Tree-walker (`lang-eval`):** `Closure` carries `defaults: Vec<Option<Expr>>` parallel to `params`. `call_closure` / `call_method_on` accept an arity in `[required, total]`; for each missing trailing parameter they evaluate its default in a child of the closure's `captured` (definition) scope and bind it — which is automatically capture-aware for a closure, since its `captured` scope is the enclosing lexical environment.
- **VM (`lang-vm` / `lang-compiler` / `lang-bytecode`):** each default expression compiles to a zero-parameter **thunk proto** carrying the function's own upvalue layout; the enclosing `Chunk` gains `defaults: Vec<(reg, thunk_proto)>` mapping a parameter register to the thunk that fills it. `Op::Call`, `Op::CallMethod`, and `call_value` accept the arity range and, for each defaulted register `>=` the supplied count, run the thunk (`run_thunk` → `self.run` on a fresh single-frame stack, like `map`/`filter` callbacks) and place the result. The thunk is handed the calling closure's captured upvalue cells, so a capture-referencing default reads the right cell; for a top-level function or method the upvalue layout is empty and the thunk resolves globals only.

## Closures (capture-aware)

A closure default is evaluated in the closure's captured scope, like the closure body, so it may reference captured variables. The wrinkle is on the VM side: a default that references a captured variable the closure body never otherwise names would not, by default, be in the closure's upvalue set — `MakeClosure` would not capture a cell for it. So **`freevars::free_vars` now also scans each parameter's default** (one enclosing layer out, since a default cannot see sibling parameters), which both adds the default's captures to the closure's upvalue set and cells them in the enclosing frame. The default thunk is then compiled with that same upvalue layout and reads the captured cells at call time. The tree-walker needs no analogue — it captures the whole environment by reference. Covered by `defaults/closure_capture_default.lang` (a default referencing a capture the body never names) and a miri-checked VM unit test for the thunk's upvalue-retain path.

## Diagnostics

- New: **`E0026 RequiredAfterOptional`** — a required parameter follows one with a default value.
- Reused: `E0007 TypeMismatch` (default value's type does not match the parameter type; also an over-arity call), `E0005 UnknownName` (a default references a non-global, e.g. another parameter), `E0022 MissingSignature` (a defaulted parameter still needs its type).
- Append-only note: this consumes **E0026**; the next free code becomes **E0027** (S5 trait coherence is renumbered from its previously-reserved E0026 to E0027).

## Oracle posture

The checker is shared, so all new static rejections are identical on both backends. The fill mechanism is implemented natively in each backend (no new `Unsupported` surface), so `--differential` stays at **0 skipped**. New conformance cases cover: a free function, an associated function, an instance method, and a closure each omitting a default; a capture-aware closure default; trailing-only violation (`E0026`) on both a function and a closure; default type mismatch (`E0007`); a default that references a parameter (`E0005`); and over-/under-arity calls.

## Verification (before commit)

- `cargo run -q -p lang-cli -- test --differential` → matched / 0 skipped / backends agree.
- `cargo run -q -p lang-cli -- test` → full conformance green (count grows).
- `cargo test` workspace-wide (new parser/checker/eval/vm unit tests; disassembly snapshots updated).
- `cargo clippy --all-targets` + `cargo fmt --all --check`.
