# P2.6 — by-name invocation (`invoke(recv, name, args)`)

Status: **DONE** (conformance 197 / differential 191 / 0-skipped / backends agree / miri-clean).
Branch `types-inferred-static`. Slice of `plans/attributes/pass-2-reflection.md`.

The single fallible invocation primitive of the reflection story (§9.13): dispatch a
method/associated-function **by a runtime string name**, returning `Result`. This is the
`call_user_func_array` foundation — the genuinely-dynamic minority's entry point. A future
member-handle API (`method.invoke(obj, args)`) would desugar *into* this; it is not built here.

## Surface (decided with the user)

`invoke(recv, name, args)` — a **keyword** builtin, symmetric with `type_of(...)` /
`attributes_of::<T>()`. Chosen over `recv.invoke(...)` / `recv.call(...)`+`Type.construct(...)`
because: (1) it is the honest primitive, not a method costume over the same dispatch; (2) it
burns one identifier (like the existing reflection keywords) instead of reserving receiver-method
names that would silently shadow user methods; (3) it makes the one surviving runtime-dispatch
site *visibly* dynamic and fallible in a static-by-default language; (4) it is the clean substrate
the future member-handle API desugars into. The "more complete" feel of the method spellings was a
mirage — none of the three options provide a first-class member handle (that is separate, later
work layered on top).

```
r = invoke(shape, "area", [2, 3]);   // instance method on a value  -> Result<dyn, dyn>
c = invoke(Circle, "new", [3]);      // associated fn on a type handle
match invoke(x, name, args) { Ok(v) => ..., Err(e) => ... }
```

- `recv`: a **value** (→ instance method) or a **bare type name** (→ associated function).
- `name`: a `string` (runtime). A non-string at runtime → `Err`.
- `args`: a `List<dyn>` (runtime). A non-list at runtime → `Err`.
- **Semantics:** unknown name → `Err(msg)`; arity mismatch → `Err(msg)`; success → `Ok(retval)`.
  A panic *inside* the invoked body is a genuine abort and propagates (only the by-name
  *resolution* — name lookup + arity — is caught). Static type is always `Result<dyn, dyn>`.

## Mechanism (one new dynamic-dispatch op + a first-class VM type value)

**Tree-walker** already has `Value::Type(Rc<TypeDef>)` and dispatches by string name in
`call_method`; `invoke` reuses that lookup but **pre-checks** name/arity to return `Err` instead of
recording a diagnostic (a dedicated `invoke_dynamic` helper). A bare type name already evaluates to
`Value::Type` (types are declared in scope), so the receiver expression is evaluated normally.

**VM** compiles associated calls *statically* and has **no runtime type value** — both gaps this
slice closes:
- `Payload::Type(String)` + `Value::type_value` / `Value::type_name_of` — the first-class type
  handle (modeled on the existing leaf `Payload::NativeModule(String)`; no heap children).
- `Op::TypeValue { dst, name }` — materialize a type handle. The `invoke` lowering emits it when
  the receiver is a bare type-name ident; otherwise the receiver compiles as an ordinary expr
  (yielding an object). (General `x = Circle` type-as-value is *not* in scope; the dynamic
  store-then-invoke form is left out of the corpus, so `0 skipped` holds.)
- `Op::Invoke { dst, recv, name, args, ok_shape, err_shape, span }` — the dynamic dispatch. `recv`
  holds an object (→ instance, `(shape.name, method)`) or a type handle (→ associated,
  `(type_name, method)`); both keyed into the existing `methods` table → proto. Miss/arity build
  `Result.Err(string)` inline (via the baked `err_shape`). On success it pushes a normal call frame
  whose `ret_transform = RetTransform::WrapOk(ok_shape)` wraps the returned value in `Result.Ok`
  at frame return (the refcount of the raw return transfers into the enum payload). `ok_shape`/
  `err_shape` are resolved at compile time via `builtin_enum_shape("Result", ...)`.

Both backends build identical `Result` values (the `MakeEnum`/`builtin_enum` precedent), so the
differential holds by construction. No new diagnostic code — invocation failures are runtime
`Result::Err`, not static diagnostics. Next free code stays **E0031**.

## Touch list

- **Lexer** `lang-lexer`: `#[token("invoke")] InvokeKw` + name/describe (mirror `TypeOfKw`).
- **AST** `lang-ast`: `Expr::Invoke { recv, name, args: Box<Expr>, span }` + `span()` arm; pretty
  `(invoke <span> <recv> <name> <args>)`.
- **Parser** `lang-parser`: `just(InvokeKw).ignore_then( "(" expr "," expr "," expr ")" )`.
- **Checker** `lang-check`: synth arm → `Result<dyn, dyn>`; bare type-name recv is licensed (not
  synthesized as a value); `name`/`args` synthesized leniently (any type — runtime-checked).
- **lang-value**: `Payload::Type(String)`; `Value::type_value`/`type_name_of`; display `<type N>`,
  json, trace/release as a leaf.
- **lang-bytecode**: `Op::TypeValue`, `Op::Invoke` + disasm.
- **lang-compiler**: lower `Expr::Invoke`; `freevars` recurse `recv`/`name`/`args`.
- **lang-vm**: `RetTransform::WrapOk(Rc<Shape>)`; dispatch `Op::TypeValue` + `Op::Invoke`.
- **lang-eval**: `Expr::Invoke` arm + `invoke_dynamic` helper.
- **Conformance** `tests/conformance/reflection/`: instance-method success, associated success,
  unknown-name → `Err`, wrong-arity → `Err`, non-string name → `Err`, the returned value matched
  with `match`/`?`. Parser snapshot. Checker unit test (synth → `Result<dyn,dyn>`).
- **Docs**: `docs/resources/02-syntax.md` reflection block; memory (`attribute-system`, pass-2 plan).

## Verification (before commit)

- `cargo run -q -p lang-cli -- test` → conformance green (count grows).
- `cargo run -q -p lang-cli -- test --differential` → matched / **0 skipped** / backends agree.
- `cargo test --workspace` · `cargo clippy --all-targets` · `cargo fmt --all --check` → clean.
