# Slice F1 — true upvalues (closures capturing enclosing locals)

Status: done

Closes the first three rows of the **VM completeness** section of `plans/deferred.md` (the M1.2/M1.5 closure-capture cluster). The compiler currently returns `Unsupported` — which makes the differential harness *skip* the program — for any closure or nested `fn` that references a binding from an enclosing function, and for a bare reassignment to a non-local inside a function. This slice adds real upvalue machinery to the VM so those programs run on both backends and stay under the `0 skipped` gate.

## Goal

A nested closure or `fn` may capture and mutate an enclosing function's locals (and forward them through intermediate functions), and a function may reassign an outer/global binding — byte-identical on the tree-walker and the VM.

## Scope

- In: closures/nested `fn`s capturing enclosing **function** locals/params; transitive capture through intermediate functions; reassigning a captured local from inside a closure (mutability enforced); reassigning a top-level global from inside a function (mutability enforced via a module-global pre-pass).
- Out (kept `Unsupported`, narrowed registry rows): a closure **inside a method** capturing `self` or a field (method-context capture); a reference to a prelude value/builtin as a value — that is slice F2 (native-function values).

## Design

The tree-walker captures the lexical scope chain by `Rc`, so captured bindings are live and shared. The only VM model that matches is **closed cells** (Lua-style, but always closed since each frame owns its registers):

- `Payload::Cell(Value)` — a mutable single-slot heap box; a captured local lives in a cell. A GC **node** with one child.
- `Payload::Closure { proto, upvalues: Vec<Value> }` — a closure owns one reference to each captured cell. Closures become GC **nodes**, so a closure→cell→closure cycle is now reclaimed by the trial-deletion collector (the collector's reason for existing).
- New ops: `MakeCell { dst, src }`, `CellGet { dst, cell }`, `CellSet { cell, src }`, `UpvalueGet { dst, index }`, `UpvalueSet { index, src }`; `MakeClosure` gains `captures: Vec<CaptureFrom>` where `CaptureFrom = Local(Reg) | Upvalue(u16)`.
- Compiler closure-conversion driven by a pure `free_vars` AST pass: a function cells exactly the locals that some descendant captures; its own ordered upvalue list (sorted, so both backends agree) is provided by its parent, which sources each from a celled local or its own upvalue. A module-global mutability pre-pass lets a function resolve/write globals with the right `mut` check.

The VM `Frame` gains `upvalues: Vec<Value>` (the closure's cells, retained on call, released on teardown).

## Checklist (vertical slice)

- [x] Bytecode: cell/upvalue ops + `MakeClosure` captures (lang-bytecode)
- [x] Value/heap: `Cell` payload, `Closure` node, GC `free`/`children` (lang-value; miri)
- [x] Compiler: `free_vars`, module-global pre-pass, celled locals + upvalue resolution (lang-compiler)
- [x] VM op: frame upvalues, cell/upvalue ops, `MakeClosure` capture (lang-vm)
- [x] Tree-walker: reference semantics already present — verified, no change needed (lang-eval)
- [x] Conformance cases (closures/{capture_param, counter_nested_fn, transitive_capture, global_mutate_from_fn, recursive_nested_fn, capture_immutable_error})
- [x] Differential green (0 skipped), miri green, fmt/clippy clean

## Definition of done — met

Six new `closures/` conformance cases run on both backends with zero divergence; the differential climbed 96 → 102 matched at **0 skipped** (107 conformance pass). lang-value miri is green, including new `cell`/`closure`-upvalue unit tests, and lang-gc gained an explicit closure→cell→closure cycle-reclamation test. fmt/clippy clean. The three closure-cluster registry rows in `plans/deferred.md` are struck; the remaining tail (method-context capture; prelude-value-as-value → slice F2; forward/mutual nested-`fn` capture) is recorded there.

## Outcome notes / design deviations

- **Closed cells, not open upvalues.** Each frame owns its register file (not a shared stack), so there is no "open upvalue pointing at a live stack slot" phase — a captured local is boxed into a heap cell at its binding site and stays closed. Simpler and a clean match for the tree-walker's `Rc`-shared bindings.
- **`lang-value` now models two new heap node types** (`Payload::Cell`, and `Closure` gained an `upvalues: Vec<Value>`), making closures GC *nodes*: a self-recursive nested `fn` forms a closure→cell→closure cycle that the trial-deletion collector reclaims (finally exercising it for its designed purpose).
- **Capture analysis is context-sensitive** because of the bare-assignment rule (`x = v` reassigns an enclosing binding if one exists). `freevars.rs` threads the enclosing-function locals and module globals to decide locality; any misclassification surfaces immediately as a differential divergence, so the oracle is the safety net.
- **Method-context capture deliberately deferred:** a closure inside a method capturing `self`/a field stays `Unsupported` (skipped), via a `forbidden` set — narrower than the old "any enclosing local" skip.
