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
| L1.3 | bundle differential oracle | L1.2 | For every corpus program: assert `run-from-source` `RunResult` ≡ `run-from-.noeb`. Add to `noeta-conformance` alongside the backend differential; keeps `0 skipped`. Runs the encrypted path too (L1.4) with a test key, so encryption is proven transparent. |
| L1.4 | **opt-in encrypted bundle** | L1.1 + L1.3 | Wrap the serialized blob in an authenticated-encryption layer keyed by a secret from the environment (see *Security model* below). Off by default (plain `.noeb`); on when a key is present / `--encrypt` is passed. Transparent to semantics: decrypt → deserialize → identical `Module`. |

**Outcome of Level 1:** a `.noeb` you ship instead of `.noe`. Plain bytecode is *not source* but
**is disassemblable** (`noeta dump` prints opcodes/constants) — obfuscation-grade. L1.4 adds an
opt-in encryption layer on top. Also a startup-cache win: skips the whole front-end.

## Security model (L1.4 — opt-in encryption)

**Decision (user, 2026-07-07): obfuscation is wanted, as an opt-in layer keyed by a salt/key set in
the environment (`.env` or an env var).** Design:

- **Build:** `noeta build --encrypt` (or auto when the key env var is set) reads the secret, draws a
  fresh random salt + nonce (`getrandom`, already in-tree), derives an AEAD key via a KDF, and
  encrypts the serialized `Module`. Artifact layout: `[magic | version | enc-flag | salt | nonce |
  AEAD(ciphertext‖tag)]`. **The salt and nonce live in the header (they are not secret); the secret
  itself is never in the artifact.**
- **Run:** the runtime reads the same secret from the environment, re-derives the key with the
  stored salt, and decrypts+verifies before deserializing. A missing/wrong key or a tampered blob
  fails the AEAD tag → a clean diagnostic (new `E00xx`), never arbitrary execution.
- **Transparent to semantics:** decryption yields the identical `Module`, so the L1.3 bundle
  differential still holds — the oracle runs the encrypted path with a test key.

**Honest threat model (state it plainly — this is obfuscation + leaked-artifact protection, not
DRM):**
- The runtime *must* have the key at run time to execute. So the protection depends entirely on
  **where the key lives**:
  - **Key is deploy-provided** (env var on the server, *not* shipped with the artifact) → a stolen
    **artifact alone is inert**. This is the meaningful, real protection.
  - **Key/`.env` ships alongside** the artifact → pure obfuscation: it defeats `noeta dump` and
    casual inspection, but anyone with both can recover the `Module`.
- An attacker who controls the **run host** can always recover the decrypted `Module` from memory —
  no local-execution scheme escapes this. AEAD is chosen so the layer *also* gives tamper-detection
  (bonus over confidentiality alone).

## Level 2 — self-contained executable (one file, no separate interpreter)

| # | Slice | Depends | Notes |
|---|---|---|---|
| L2.0 | embedded-blob bootstrap | L1.2 | At startup the runtime checks for an embedded `.noeb` (trailer with `[magic][offset]` appended to its own executable, read via `std::env::current_exe`); if present, run it; else behave as the normal CLI. Trailer-append is the portable approach (no per-OS section surgery). |
| L2.1 | `noeta build --exe -o app` | L2.0 | Concatenate a copy of the runtime binary + the blob + trailer → a single executable. No `.noe`, no separate `noeta` install. Still bytecode under the hood. Composes with L1.4: the embedded blob can be the **encrypted** one, so a leaked `app` is inert without the deploy key (which is still read from the environment at launch, never embedded). |

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

**Encryption × Level 3:** an AOT binary's **native machine code** is inherently opaque (not
bytecode — `noeta dump` can't read it), but it is *not* encrypted; the L1.4 layer applies to the
**embedded bytecode-fallback blob** that carries the ~75% of ops the JIT doesn't lower natively. So
a Level-3 binary is "opaque native + optionally-encrypted bytecode fallback." The same honest
threat model holds (the process must run the code, so a host-controlling attacker can still observe
it).

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
- ~~**Threat model**~~ **RESOLVED (user, 2026-07-07):** obfuscation wanted, as an **opt-in
  encryption layer** (L1.4) keyed by a salt/key from the environment. Remaining sub-decisions,
  each with a recommendation:
  - **AEAD cipher:** `chacha20poly1305` (RustCrypto — pure-Rust, constant-time, no AES-NI dependency;
    matches the rustls/pure-Rust posture). New workspace dep. *Recommended over `aes-gcm`.*
  - **KDF:** `argon2` (argon2id) for a human **passphrase** (memory-hard — the salt is exactly what
    it consumes), with `hkdf`-SHA256 (over the in-tree `sha2`) for a **raw 32-byte key**. New deps:
    `argon2` (+ `hkdf`). *Support both; detect by key length/encoding.*
  - **Secret source:** one env var (e.g. `NOETA_BUNDLE_KEY`) as the primary; a project-root `.env`
    loaded by the CLI as a convenience (via `dotenvy`, a tiny standard crate — none in-tree today).
    *Recommend env-var-primary so the key can be deploy-provided and kept out of the artifact.*
  - **Key hygiene:** `zeroize` the derived key after use. Small dep, worth it.
  - **New diagnostic:** a decrypt-failed / wrong-key `E00xx` (next free code — check the catalog at
    implementation time; the P-JCT/crypto arcs advanced it).
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
