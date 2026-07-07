# AOT & bundling arc — source-free binaries (P-AOT)

**Status: PLANNED (not started).** Goal: ship a runnable artifact that does **not** include the
`.noe` source. Three levels of increasing ambition and cost — bytecode bundle → self-contained
executable → native AOT + DCE. Levels 1–2 deliver the source-hiding goal cheaply and hand Level 3
its interpreter-fallback substrate for free; Level 3 is the performance/opacity milestone.

Provenance: design conversation 2026-07-07 (after the P-JCT JIT compile-throughput arc). The user
confirmed **JIT op-coverage expansion is deferred to a separate later track** — AOT ships at
today's coverage via the interpreter fallback; coverage is shared work that lifts both tiers and is
sequenced independently (see *Out of scope*).

## What exists today (verified against the repo, 2026-07-07)

- **The pipeline recompiles from source every run.** `noeta-cli::compile_real` (`main.rs:226`)
  lexes/parses/checks/compiles a `.noe` into a `noeta_bytecode::Module`; `cmd_run` (`main.rs:316`)
  loads it onto the VM. There is **no `build`/`compile`/`bundle` command** — only
  run/test/bench/doc/dump/repl/lsp/dap.
- **`Module` is plain owned data.** `Module` (`noeta-bytecode/src/lib.rs:1190`) holds `Vec<Chunk>`,
  `Vec<Shape>`, constants, method/destructor/derive tables, `reflection: ReflectionInfo`, and R1
  `TypeRepr` tables. A scan for `Rc`/`Arc`/`*const`/`*mut`/`Box<dyn>`/fn-pointers across
  `noeta-bytecode`, `noeta-object` (`Shape`), and `noeta-ast::reflect` (`ReflectionInfo`,
  `TypeRepr`) finds **none** — every constituent is serde-derivable with a shallow sweep.
  `noeta-span` (`Span`/`SourceId`/`Source`) **already** derives `Serialize`/`Deserialize`.
- **The interned-`&'static Shape` handles are a VM-load-time concern, not a Module field.** `Module`
  stores owned `Vec<Shape>`; the VM/JIT interns them (`noeta_object::intern_shape`) *when it loads a
  Module*. So a deserialized Module re-interns on the existing load path — serialization sidesteps
  the leaked-pointer wrinkle entirely. (This corrects the initial design-chat framing, which
  over-weighted it.)
- **The JIT is already a hybrid** — native for ~a quarter of the 125 `Op` variants (`is_fast_op`:
  arithmetic/comparison `Binary`, `Move`/`Drop`, globals, `Call`/`Return`, branches, ~9 leaf heap
  ops), and **every uncovered op bails to the interpreter mid-frame and resumes native** via
  resume-pcs. AOT inherits this model: it does **not** need full coverage to function; it embeds the
  interpreter as the fallback, exactly as the JIT does at runtime.
- **`noeta.toml` build profiles already parse** (`noeta-cli::manifest`), resolving a profile to an
  active-tier set. A production build naturally strips `@test`/`@debug`/`@doc` tier blocks *before
  lowering* (they never reach the Module), so a bundled artifact excludes that content by
  construction — no DCE needed for tier content.
- **The M3 "startup cache" roadmap item is subsumed** by Level 1 (a cached serialized Module *is* a
  startup cache); this arc delivers it as a side effect.

## Posture (inherited from the perf/M2 arcs)

- **New differential oracle: bundle-run ≡ source-run, byte-identical.** Every corpus program run
  from its serialized/AOT artifact must produce a `RunResult` identical to running it from source.
  This is the safety spine — the existing `SandboxHost` determinism makes it well-defined. Wired
  into conformance so `0 skipped` still holds.
- Determinism unchanged (seeded RNG, logical clock, sorted iteration). No wall-clock in output.
- Gates per slice: conformance, both differentials (backend + bundle), `cargo test --workspace`,
  clippy, fmt. Any codegen slice (Level 3) also re-runs the jit-differential and leak oracle.
- **Commit per green slice; never push without authorization.** Work in a dedicated branch/worktree.

## Level 1 — bytecode bundle (the cheap win: no `.noe` shipped)

Serialize the compiled `Module`; ship the blob; run it directly, skipping the front-end.

| # | Slice | Depends | Notes |
|---|---|---|---|
| L1.0 | serde the bytecode graph | — | `#[derive(Serialize, Deserialize)]` sweep over `Op`/`Chunk`/`Const`/`Module` (noeta-bytecode), `Shape` + packed schemas (noeta-object), `ReflectionInfo`/`TypeRepr` (noeta-ast::reflect). `Span` is already done. Round-trip prop-test: `Module == deserialize(serialize(Module))` over the corpus. Risk: shallow-but-wide; the only unknowns are any nested enum in `ReflectionInfo` (verify each has no non-serde field). |
| L1.1 | versioned `.noeb` artifact format | L1.0 | Header = magic bytes + a **runtime-version** word + payload. Loader rejects a version mismatch with a clear diagnostic (simplest correct policy: artifacts are pinned to the runtime that built them — no cross-version compat guarantee in v1). Serializer lib = **decision point** (see below). |
| L1.2 | `noeta build <file> -o app.noeb` | L1.1 | Runs `compile_real` honoring the resolved `noeta.toml` build profile (so tier blocks strip), then serializes. `noeta run app.noeb` sniffs the magic and loads the `Module` straight onto the VM, bypassing lex/parse/check/compile. Source is never read at run time. |
| L1.3 | bundle differential oracle | L1.2 | For every corpus program: assert `run-from-source` `RunResult` ≡ `run-from-.noeb`. Add to `noeta-conformance` alongside the backend differential; keeps `0 skipped`. |

**Outcome of Level 1:** a `.noeb` you ship instead of `.noe`. Bytecode is *not source* but **is
disassemblable** (`noeta dump` already prints opcodes/constants) — obfuscation-grade, not
encryption (see the threat-model decision point). Also a startup-cache win: skips the whole
front-end.

## Level 2 — self-contained executable (one file, no separate interpreter)

| # | Slice | Depends | Notes |
|---|---|---|---|
| L2.0 | embedded-blob bootstrap | L1.2 | At startup the runtime checks for an embedded `.noeb` (trailer with `[magic][offset]` appended to its own executable, read via `std::env::current_exe`); if present, run it; else behave as the normal CLI. Trailer-append is the portable approach (no per-OS section surgery). |
| L2.1 | `noeta build --exe -o app` | L2.0 | Concatenate a copy of the runtime binary + the blob + trailer → a single executable. No `.noe`, no separate `noeta` install. Still bytecode under the hood. |

**Cross-compilation** (building an `app` for a different target triple) is **out of Level 2 v1** —
it ships the host-target runtime. Flagged as a later extension (needs prebuilt per-target runtime
binaries to staple onto).

## Level 3 — native AOT + DCE (the backend milestone)

Compile functions to machine code ahead of time and link a standalone binary. Reuses the JIT's
Cranelift codegen; the covered subset goes native, the rest runs through the **embedded interpreter
from Levels 1–2** (the same hybrid the JIT uses at runtime).

| # | Slice | Depends | Notes |
|---|---|---|---|
| L3.0 | generalize codegen over `cranelift-module::Module` | — (parallel to L1/L2) | The keystone reuse. Today `Jit` hardcodes `cranelift-jit::JITModule`; abstract the module backend so the *same* `emit_int_body`/`FunctionBuilder` IR construction can target `cranelift-object::ObjectModule`. **Gate: the JIT path stays byte-identical** (jit-differential green) — this is a refactor, not a behavior change. |
| L3.1 | AOT compile driver | L3.0 | Eagerly compile **every** proto (not just hot ones) to an object file. Runtime helpers (`retain`/`release`/`call`/`return`/…) resolved by **linking against the runtime crate**, not baked addresses. Emit relocations for direct native→native calls. |
| L3.2 | link driver → standalone binary | L3.1 + L2.1 | Link the object + the runtime + the embedded interpreter/bytecode substrate (Levels 1–2) into one binary: native for the covered ops, interpreter fallback for the rest. |
| L3.3 | AOT differential oracle | L3.2 | AOT-binary `RunResult` ≡ source-run, over the corpus. |
| L3.4 | DCE / tree-shaking | L3.2 | **Own sub-milestone, optional for shipping L3.** Whole-program reachability drops unused functions + stdlib. Hard part = dynamic dispatch (trait method tables, `invoke`, reflection `attributes_of`/`type_of`, the stdlib registry): needs conservative roots (`@reflectable`) and closes the deferred "reflection-metadata elimination" row (`deferred.md`, gated on exactly this). Aggressiveness = decision point. |

## Sequencing (value × independence, ascending risk)

1. **Level 1 first** — pure plumbing, no codegen, directly achieves the source-hiding goal. L1.0
   (serde sweep) is the foundation; L1.3 (bundle oracle) is the safety gate every later level reuses.
2. **Level 2** — a small delta on L1 (bootstrap + stapling). Completes "one binary, no scripts."
3. **Level 3** in order L3.0 → L3.1 → L3.2 → L3.3, then **L3.4 DCE last** (optional — L3 ships
   without it, just with larger binaries). L3.0 is the one real reuse of JIT codegen and carries its
   own no-regression gate.
4. Coverage expansion runs on its **own** track throughout, consumed by both tiers (out of scope
   here).

## Decision points (surface before committing the affected slice — do not silently pick)

- **Serialization library** (L1.1): `bincode` (fast, compact, ubiquitous), `postcard` (no_std-lean,
  stable wire format), or hand-rolled. *Recommendation: `postcard` for a stable, versioned wire
  format; revisit if a Module-specific format buys meaningful size.*
- **Threat model** (L1/L2): bytecode is disassemblable — is obfuscation the bar, or is
  **encryption/signing** wanted (an encrypt-at-build / decrypt-at-load layer, or a signature to
  prevent tampering)? This changes scope; *default = obfuscation-grade (no crypto layer) unless you
  say otherwise.*
- **Artifact/version compat** (L1.1): pin artifacts to the building runtime and reject mismatches
  (simplest, recommended v1), or invest in a stable cross-version wire format now?
- **DCE aggressiveness + reflection root policy** (L3.4): how much to strip under dynamic dispatch;
  `@reflectable` as the opt-in root set.
- **Cross-compilation** (L2/L3): host-target only in v1, or prebuilt per-target runtimes now?

## Explicitly out of scope (deferred — user-confirmed 2026-07-07)

- **JIT op-coverage expansion.** AOT functions at today's ~25% coverage via the interpreter
  fallback; expanding coverage (bitwise/`WideInt`, the rest of the collection/map/string leaves, ADT
  construction, `match` dispatch, closures/upvalues, async ops) is a **separate shared track** that
  lifts both the JIT and AOT and is sequenced independently. It raises the AOT *quality* ceiling
  (how much of an eagerly-compiled program is native vs interpreted) but is neither a prerequisite
  nor unique to AOT.
