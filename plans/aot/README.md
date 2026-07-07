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
| L1.0 | serde the bytecode graph | — | ✅ **DONE** (`2a3ed41`). Serde sweep over the whole graph (Op/Chunk/Const/Module, Shape/ShapeKind, ReflectionInfo/TypeRepr/AttrArg/AttrValue/BinaryOp/UnaryOp, IntMethod/TypeRecipe) — all plain owned data; `Module` keeps owned `Vec<Shape>`, the interned `&'static` runtime types (`noeta_object::PackedSchema`/`PackedKind`) are *not* reachable and stay non-serde. `Module::encode`/`decode` via postcard. Corpus round-trip oracle: 494 modules byte-stable, 0 skipped. |
| L1.1 | versioned `.noeb` artifact format | L1.0 | ✅ **DONE** (`92091bd`). New `noeta-bundle` crate (isolated from the mid-end so future compression/crypto stays out of core): `[magic NOEB | fmt_ver | flags | rt_len | rt_ver | payload]`; `rt_ver` pins artifacts to their builder (mismatch = clear error); `flags` reserves compressed/encrypted bits (a set bit is rejected in v1). `write`/`read`/`is_bundle`. |
| L1.2 | `noeta build <file> -o app.noeb` | L1.1 | ✅ **DONE** (`57ec769`). `noeta build` compiles via `compile_real` (tiers stripped unless `--tier`/`--profile`), serializes. `noeta run app.noeb` sniffs the magic and runs the module directly — no source, no compile/check; build-time flags rejected. Aborts render against a synthetic empty source (message/code/trace show, no snippet). 2 CLI integration tests. |
| L1.3 | bundle differential oracle | L1.2 | ✅ **DONE** (`f7047a1`). Beyond L1.0's byte round-trip, the *decoded* module runs byte-identically to the source-compiled one on the sandbox (stdout/exit/diagnostics). 494 modules round-trip AND run identically; 0 skipped. |
| L1.4 | **obfuscation (default, no key)** | L1.1 + L1.3 | The default `.noeb` is not plaintext bytecode: **compress** the serialized `Module` (size win + defeats `strings`/`grep`) and apply a light reversible transform, so `noeta dump` / casual inspection / automated tooling fail on the shipped file. No key, no env, no distribution friction — it just works like any binary. Honestly labeled as obfuscation, not security (see below). |
| L1.5 | **optional keyed encryption** | L1.4 | Opt-in (`--encrypt`, off by default) authenticated-encryption layer keyed by a secret from the environment, for the *untrusted-distribution* case only. The one mode with real leaked-artifact protection; adds key-distribution cost. Transparent to semantics: decrypt → deserialize → identical `Module`. |

**Outcome of Level 1:** a `.noeb` you ship instead of `.noe`. By default it is **obfuscated**
(compressed + scrambled — not human-readable, not `noeta dump`-able, no key to manage);
`--encrypt` optionally adds a keyed layer for untrusted distribution. Also a startup-cache win:
skips the whole front-end.

## Security model — obfuscation by default, keyed encryption optional

**Decision (user, 2026-07-07): the goal is obfuscation without a required key** — key-based
encryption complicates distribution (the key must reach every run environment), and the honest
reality is that *any* scheme where the runtime decrypts-and-runs on its own has the key effectively
embedded, so it is obfuscation regardless. So:

- **Default (L1.4) — obfuscation, no key.** The shipped artifact is compressed + lightly scrambled:
  not plaintext, not `strings`-able, not `noeta dump`-able, defeats casual inspection and automated
  tooling. **Zero distribution friction.** Deliberately *not* built on the crypto stack — dressing a
  baked-in key up as AEAD would be security theater (looks like encryption, provides
  obfuscation-level protection). What it does **not** stop: a determined reverser (the
  de-obfuscation algorithm is in the open-source runtime, and the `Module` is recoverable from
  process memory at run time). That is the accepted, stated bar for the default.
- **Optional (L1.5) — keyed encryption, external key.** Earns its complexity in exactly one
  scenario: **distributing the artifact to untrusted third parties** (customers / on-prem clients)
  where a leaked copy should be inert. Build-time draws a random salt + nonce, derives an AEAD key
  from an env-provided secret, encrypts the serialized `Module`; header carries `[magic | version |
  flags | salt | nonce | AEAD(ciphertext‖tag)]` — salt/nonce are not secret, the **key is never in
  the artifact**. Run-time re-derives from the same env secret and decrypts+verifies (wrong key /
  tamper → clean `E00xx`, never arbitrary execution). Real protection **only** when the key is
  deploy-provisioned and kept out of the artifact — if the `.env` ships alongside, it collapses back
  to obfuscation. And a run-host-controlling attacker can still dump the decrypted `Module` from
  memory; no local-execution scheme escapes that.

**Rule of thumb:** deploying to *your own* servers → obfuscation (default) is the right call, no key
management. Shipping to *someone else's* machines and a leaked artifact matters → `--encrypt` with a
deploy-provisioned key.

## Level 2 — self-contained executable (one file, no separate interpreter)

| # | Slice | Depends | Notes |
|---|---|---|---|
| L2.0 | embedded-blob bootstrap | L1.2 | At startup the runtime checks for an embedded `.noeb` (trailer with `[magic][offset]` appended to its own executable, read via `std::env::current_exe`); if present, run it; else behave as the normal CLI. Trailer-append is the portable approach (no per-OS section surgery). |
| L2.1 | `noeta build --exe -o app` | L2.0 | Concatenate a copy of the runtime binary + the blob + trailer → a single executable. No `.noe`, no separate `noeta` install. Still bytecode under the hood. The embedded blob is obfuscated by default (L1.4); with `--encrypt` (L1.5) it is the encrypted one, so a leaked `app` is inert without the deploy key (read from the environment at launch, never embedded). |

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

**Obfuscation/encryption × Level 3:** an AOT binary's **native machine code** is inherently opaque
(not bytecode — `noeta dump` can't read it), so Level 3 raises the default bar for free. The
embedded **bytecode-fallback blob** (the ~75% of ops the JIT doesn't lower natively) is obfuscated
by default (L1.4) and optionally encrypted (L1.5). So a Level-3 binary is "opaque native + obfuscated
(or encrypted) bytecode fallback." The same honest threat model holds — the process must run the
code, so a host-controlling attacker can still observe it.

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
- ~~**Threat model**~~ **RESOLVED (user, 2026-07-07):** **obfuscation by default, no key** (L1.4);
  keyed encryption is **optional** (L1.5) for untrusted distribution only. Sub-decisions:
  - **Obfuscation transform (L1.4, the default path):** compression choice — `zstd` (best ratio +
    speed) or `flate2`/deflate (lighter dep). *Recommend `zstd`* (size win doubles as the scramble;
    add a light reversible byte transform so it isn't literally "unzip it"). No crypto deps on this
    path — keeping it honestly labeled as obfuscation.
  - **AEAD cipher (L1.5, only if keyed encryption ships):** `chacha20poly1305` (pure-Rust,
    constant-time, no AES-NI dependency; matches the rustls posture). *Recommended over `aes-gcm`.*
  - **KDF (L1.5):** `argon2` (argon2id) for a passphrase, `hkdf`-SHA256 (over in-tree `sha2`) for a
    raw 32-byte key; `zeroize` the derived key. Secret from a `NOETA_BUNDLE_KEY` env var (primary),
    optional project `.env` via `dotenvy`. New deps land **only if L1.5 is built.**
  - **New diagnostic (L1.5):** decrypt-failed / wrong-key `E00xx` (next free code at implementation
    time).
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
