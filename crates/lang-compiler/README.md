# lang-compiler

The bytecode compiler: AST → `Module`.

- **Takes in:** an AST `Program` (from `lang-ast`); `PRELUDE_NAMES` (from `lang-builtins`).
- **Emits:** a `lang-bytecode` `Module` (the prototype table), or `Unsupported` for any construct outside the VM's current subset (the differential harness skips those).

Lowers literals, bindings (`mut`/immutable + reassignment), `echo`, unary/binary arithmetic, comparison, short-circuit logic, `~` concatenation, **functions** (`fn` declarations, calls, arrow closures, the `|>` pipeline, `return`, `if`/`else`), **collections** (`[...]`/`{...}` literals, `for`-in with `(i, x)` destructuring, the `len`/`map`/`filter`/`sum` builtins, `.count()`/`.enumerate()`), **string interpolation**, the **object model** (records/classes/enums on shapes, all-fields literals with `..spread`, member access, associated functions and instance methods, enum construction), and **`match`/`?`/`??`** with the `Result`/`Option` constructors and `panic`/`next_id`. As of M1.5 it lowers the whole M0 language (the Thrust-A gate: 100% of the corpus, every case differential-identical). Names resolve through a two-level model: frame-local registers for parameters/locals (a method's register 0 is the receiver, its field names resolving against it), a by-name global table for top-level bindings and function names; a nested closure may capture nothing but globals (true upvalues are deferred). A three-pass compile registers every top-level `type`/`class`/`enum`/`use` first (so forward references and shapes exist), then compiles class methods, then the top-level program. The lowering mirrors the M0 tree-walker's evaluation order and exact diagnostic text/spans so the differential oracle sees identical `RunResult`s.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
