# Cross-language micro-benchmark — Noeta vs PHP / Python / Lua / LuaJIT

Date: 2026-08-16, main `57091de12` — **247 commits after the last run, and no optimization landed
in this one.** This is a re-measurement: the seven performance rows held in `plans/backlog.md` were
held because nothing in the tree said which of them was worth doing, and the only measurement that
existed was two weeks old. Engine code is untouched; the deliverable is numbers.

**Competitor versions.** PHP 8.5.8 and Python 3.14.6 are unchanged since 2026-07-05. Two moved and
the tables have to say so: **Lua 5.5.1** (was 5.5.0) and **LuaJIT 2.1.1782726002** — LuaJIT's `loop`
went 9.6 → 14.2 ms between the two runs with nothing on our side involved, so LuaJIT-relative ratios
must be re-derived from this run's own field, never carried forward from an older table. Nothing is
missing; every competitor ran.

**Method.** Four binaries built with the CI pin (`cargo +1.97.0 build --release -p noeta-cli`, and
`--no-default-features` for the interpreter): today's `57091de12` and the previous report's
`7e7d038db`, so the A/B is the old code on today's machine rather than against a stale table. A
fifth, `95f14eeef` (pre-arc), was built to settle one standing open item. The wall-clock field was
taken on a genuinely quiet box — `uptime` load 0.05–0.16 before each field run, peaking 0.86 under
the benchmark itself, no sibling agent builds. Everything diagnostic is still **instructions
retired**, via `measure.sh` and `attrib*.sh`; `xrun3.py` reproduces the wall-clock field (its
`LANGS` table points at this run's four binaries through `BENCH_BIN_DIR`).

## Startup (empty program, ms)

| Noeta-JIT | Noeta-int | JIT@0802 | int@0802 | PHP | PHP+JIT | Python | Lua | LuaJIT |
|----------:|----------:|---------:|---------:|----:|--------:|-------:|----:|-------:|
| **1.9** | 1.5 | 1.7 | 1.5 | 17.1 | 18.2 | 11.4 | 0.9 | 0.9 |

## Compute time (ms) — lower is better

| bench | Noeta-JIT | Noeta-int | PHP | PHP+JIT | LuaJIT | Lua | Python |
|---|---:|---:|---:|---:|---:|---:|---:|
| loop 10M | 36.8 | 375.7 | 51.7 | 15.8 | **14.2** | 73.5 | 638.7 |
| fib(32) | 35.0 | 337.2 | 50.9 | 25.1 | **12.9** | 76.1 | 128.6 |
| strcat 50k | **3.4** | 3.5 | 10.4 | 10.9 | 32.7 | 38.3 | 14.9 |
| assoc 100k | 20.1 | 22.0 | **8.9** | 9.0 | 19.4 | 38.4 | 42.9 |
| wordcount 200k | 22.8 | 27.5 | 7.5 | 6.6 | **5.6** | 13.4 | 40.9 |

**Every standing conclusion holds and nothing changed rank.** Best startup among the JIT-having
languages (9.0× PHP, 6.0× Python); string building won outright (3.1× PHP, 4.4× CPython, 9.6×
LuaJIT); loops and calls clear of every non-tracing engine (1.40× and 1.45× the PHP baseline, 17.4×
and 3.7× CPython). Maps are still the one workload where PHP leads: assoc **2.3×** and wordcount
**3.0×** the PHP baseline — the same 2.2× and 2.9× the last run reported, so the answer to "are maps
still the gap" is yes, unchanged in size.

## Is tier 1 actually running on the map rows? Yes — and here is what says so

Instructions retired, min of 3, pinned to CPU 3. The JIT/interpreter ratio is the load-bearing
column: it needs no competitor and it is measured on the instrument that survives a busy box.

| bench | Noeta-JIT | Noeta-int | JIT/int | JIT@0802 | Δ | int@0802 | Δ |
|---|---:|---:|---:|---:|---:|---:|---:|
| loop 10M | 1,146.0M | 11,892.3M | **10.38×** | 1,129.3M | +1.5% | 11,802.3M | +0.8% |
| fib(32) | 941.1M | 9,041.5M | **9.61×** | 936.2M | +0.5% | 9,112.0M | −0.8% |
| strcat 50k | 62.9M | 85.7M | **1.36×** | 61.5M | +2.2% | 85.2M | +0.6% |
| assoc 100k | 333.6M | 457.5M | **1.37×** | 332.8M | +0.3% | 453.4M | +0.9% |
| wordcount 200k | 485.9M | 656.0M | **1.35×** | 483.3M | +0.5% | 650.3M | +0.9% |

No row has equal columns, so the trap the last report named does not fire here. Because one
instrument agreeing with itself is not corroboration, three others were asked the same question:

- **`--jit-stats`** reports `1 OSR loop windows` for `strcat`, `wordcount` and `loop`, `2` for
  `assoc`, and `1 of 1 compiled prototypes native` for `fib`. Bail sites are `1×` each — one native
  entry falling back at loop exit on a `LoadGlobal`/`LoadConst`, not per iteration.
- **`perf` sees the native frames directly.** `[JIT] noeta_jit_proto0_osr6_19` and
  `…_osr26_40` carry 3.9% of assoc's samples, `…_osr8_25` 2.2% of wordcount's. Anonymous
  executable pages with JIT-emitted symbols cannot appear if tier 1 never ran.
- **`scripts/perf-ratchet.sh` passes with its engine assertions intact** — `map` and `loop_jit`
  both `native`, `arith` `declined` as designed, every row inside its band (largest +0.82%).

The honest caveat is the one the JIT does *not* buy: 1.35–1.37× on the map rows, against 9.6–10.4×
on `loop`/`fib`. Tier 1 runs; it just does not reach most of the work, which is the next section.

## Where the instructions go on the slow rows

`perf record -e instructions:u` over the JIT binary, flat profile (the DWARF call-graph unwind on a
release build produced broken stacks and was discarded rather than reported):

| assoc 100k | % | wordcount 200k | % |
|---|---:|---|---:|
| `heap::alloc_with` | 25.1 | `compact_str::Repr::push_str` | 26.1 |
| `compact_str::Repr::push_str` | 16.3 | `Vm::map_update_key` | 23.0 |
| `tier1::jit_run_leaf_op` | 13.9 | `heap::alloc_with` | 17.4 |
| `itoa::Unsigned::fmt` | 6.5 | `FxHasher::hash_one::<&MapKey>` | 9.4 |
| `Value::map_get` | 3.9 | `Value::map_get` | 8.2 |
| `HashMap<MapKey, Value>::insert` | 3.7 | `tier1::jit_run_leaf_op` | 6.1 |
| `FxHasher::hash_one::<&MapKey>` | 3.4 | JIT-emitted native code | 2.2 |
| JIT-emitted native code | 3.9 | `Vm::jit_drain_service` | 1.3 |
| `heap::release` + `free` + drop glue | 4.2 | `Vm::map_update_in_place` | 1.0 |

**The map gap is a string-and-allocation gap, not a hash-table gap.** On assoc, ~48% of instructions
go into *building the key* — a heap allocation per interpolated string, a `compact_str` push, and
integer formatting — while the hash table proper (insert + get + hash) is ~11%. The control settles
it: the same benchmark with `Map<int, int>` and no string keys puts `alloc_with` below the 1.5%
cutoff entirely and is led by `dispatch_inner` at 22.9%. `assoc.noe` builds 200,000 throwaway
strings; PHP's `"key" . $i` is cheap for reasons (interned `zend_string`, arena allocation) that a
faster probe adapter does not touch.

The instruction ladder agrees on direction and is the number to quote for scale — each fixture alone
in its own directory, min of 3:

| fixture | instructions | reading |
|---|---:|---|
| `a0_ctl` — the two 100k loops, no map, no strings | 42.7M | loop scaffolding |
| `a2_intmap` — same loops, `Map<int,int>` | 241.9M | +199.2M for 200k map ops |
| `a3_assoc` — the real benchmark | 331.2M | +89.3M for string keys over int keys |
| `w0_ctl` — the 200k loop alone | 37.5M | |
| `w2_wordcount` — the real benchmark | 484.8M | |

Two things a fix would target, in this order: **the per-iteration string** (an interpolated key costs
~380–450 instructions before the map is touched), and **`map_update_key`**, which is 23% of
wordcount because `Set` always materializes an *owned* `MapKey` before `map_insert` — including on
the 199,500 iterations out of 200,000 that merely overwrite a key already in the map.

Note what is *not* on this list: the zero-allocation probe adapter. `impl Equivalent<MapKey> for str`
and `ExternKeyRef` already exist in `crates/noeta-ext-abi/src/map_key.rs`, and `Vm::map_probe`
already reads through them with a borrowed `&str` and no clone.

## The finding: a map read-modify-write copies the whole map, per iteration

`wordcount.noe` writes its key to a variable. Spell the same key inline instead and the program is
**240× slower** — and the cost scales with the map's entry count, which means the map is being copied
on every iteration. Instructions retired, 20,000 iterations, `Map<string,int>`:

| shape | 50 entries | 500 entries | growth |
|---|---:|---:|---|
| `key = …; m[key] = m.get_or(key, 0) + 1` | 63.2M | 65.9M | flat |
| `m[…] = m.get_or(…, 0) + 1` (inline both sides) | 981.3M | 11,548.8M | **11.8× for 10× entries** |
| inline on the subscript only | 981.5M | 11,548.8M | same |
| inline on the `get_or` argument only | 981.5M | 11,548.7M | same |
| `m[…] = i` — inline key, no read of `m` | 54.2M | 50.1M | flat |

It is not about strings: with `Map<int,int>` keys the same pair is 51.2M/55.3M named against
664.5M/7,352.1M inline. It is not about lists: `l[k] = l[k] + 1` is 54.7M/54.7M at both lengths. It
is not the aliasing machinery misbehaving in general — a genuine alias (`t = s; s = s ~ "x"`) costs
289.2M against 30.6M, which is the copy the semantics require.

`noeta dump` names the mechanism. Both spellings end the statement with `TakeGlobal` + `CallMethod
set [reuse]`, and `Vm::map_update_in_place` takes an in-place branch only when `map.refcount() == 1`.
In the named form the register allocator happens to give `LoadGlobal "m"` (the `get_or` receiver) and
`TakeGlobal take("m")` **the same register**, so loading the second reference releases the first. In
the inline form the extra string temporary pushes them into *different* registers, the first
reference stays live in a register nothing reads again, the refcount is 2, and the update falls to
`call_map_method` — which copies an N-entry map, 20,000 times.

Two qualifiers, both load-bearing:

- **It is not a regression.** `7e7d038db` reproduces it to four digits (664.1M / 7,351.9M). It has
  been there through every report in this file; nobody measured this shape.
- **It is top-level only.** Inside a function, where `m` is a register local rather than a global
  slot, both spellings are flat: 52.0M / 49.2M inline against 51.3M / 48.3M named.

So the in-place update guard is currently protected by register-allocation luck, and `wordcount.noe`
sits on the lucky side of it. The fix is a last-use release for dead registers in the top-level
lowering (or an explicit `Drop` of the receiver register after `CallMethod`), not a faster map.

## What moved since `7e7d038db` — nothing, and a small startup give-back

The hot paths are flat: the five compute rows sit within +2.2% in instructions retired (assoc +0.3%,
wordcount +0.5%, loop +1.5%), and the wall-clock field agrees at ±3%. 247 commits landed on the VM
with no measurable cost, which is the same result the last two reports got for the JIT column.

Startup gave a little back. Instructions retired, empty program alone in its own directory:

| | pre-arc `95f14eeef` | `7e7d038db` | main `57091de12` | vs 0802 |
|---|---:|---:|---:|---:|
| `--version` | 2,551,015 | 2,293,407 | 2,371,124 | **+3.39%** |
| `check` | 3,393,666 | 2,934,580 | 3,051,100 | +3.97% |
| `run --no-cache` | 5,580,506 | 3,180,480 | 3,296,853 | +3.66% |
| `run` (cached) | 3,881,635 | 2,313,690 | 2,341,545 | +1.20% |
| `R_X86_64_RELATIVE` | 52,892 | 50,122 | 51,521 | +1,399 |
| binary (JIT) | 51,989,912 B | 51,820,400 B | 53,344,904 B | +2.94% |

The 1,399 new relocations are ~16.8k instructions at the measured ~12 each, so they are about a
fifth of the `--version` give-back; the rest is more code running before `main`. The arc's win is
still mostly banked (`--version` is 7.05% below pre-arc, `run --no-cache` 40.9% below), but the creep
has resumed at roughly 1.4% per hundred commits and the ratchet's 1.0% band on `version`/`startup`
will catch it before it compounds — this run measured +0.18% and +0.53% against a baseline recorded
four commits ago.

Relocation ownership is unchanged in shape: `regex_syntax` 11.50% (5,927 of 51,521), then an
`<internal>` bucket at 11.40%, `noeta_stdlib` 7.46%, `aws_lc_sys` 6.25%, `reqwest` 5.95%.

## Corrections to the 2026-08-02 report

Five of its claims no longer hold, and two of its instruments are weaker than it says.

1. **"`workspace_editions` has the same unmemoized shape `workspace_packages` had"** — resolved. The
   function now carries a written argument for staying unmemoized: it has exactly one caller,
   `workspace_provenance_memo`, which is itself `#[salsa::tracked]`, so a memo buys nothing and costs
   a salsa entry per check. The doc comment names the invariant to preserve (the caller count) and
   what to do if a per-source caller ever appears.
2. **"A hot function whose loop is long still runs on the whole-prototype body — no regression, no
   gain. Closing it needs outlining."** — false today, both halves. `loop_fn.noe` (one call, 10M-iteration
   loop) compiles an OSR loop window and runs **30.7×** the interpreter: 398.3M against 12,232.2M. A
   function that is *also* call-promoted — 200 calls, 50k-iteration loop — reports `1 of 1 compiled
   prototypes native` **and** `2 OSR loop windows`, and runs 27.3× the interpreter. There is no
   "no gain" case left to outline.
3. **"Top-level list-index loops are +4.5% vs pre-arc, unattributed"** — reproduced, and it has
   roughly halved on its own. A top-level `xs[i % 1000]` loop measures 896.6M at `7e7d038db` against
   859.1M pre-arc (**+4.36%**, which is the reported number to within 0.15pp) and **879.8M today —
   +2.41%**. The string-element variant is now 6.7% *below* pre-arc, and the plain top-level `loop`
   4.5% below. Nobody targeted any of this.
4. **"Per-sibling cost now falls with directory size instead of rising"** — the quadratic is gone,
   but the per-sibling cost is *flat*, not falling: 215.8k instructions per sibling at n=48
   (13,193,187 total) and 216.3k at n=384 (85,909,398 — which reproduces the report's 85,910,230 to
   five digits). Linear, not sub-linear.
5. **The startup creep is not still "arrested and reversed"** — see the table above; `--version` is
   +3.39% since that sentence was written.

And the two instruments:

- **The tier-1 equality check in `xrun3.py` runs on wall-clock**, which is the instrument this file
  says not to trust for exactly this kind of judgment. On this run it put `strcat` **3.0% apart** —
  one tenth of a percentage point from printing `THE JIT IS NOT RUNNING HERE` on a row where
  instructions retired have the JIT 1.36× ahead. The check that exists to prevent a wrong reading is
  one noisy field run away from producing one. It should be computed from instruction counts.
- **`--jit-stats` is still partly blind in the way the report says was fixed.** Its headline line
  reads `tier 1: 0 of 0 compiled prototypes native` for *every* top-level-loop program — `loop`,
  `strcat`, `assoc`, `wordcount` — because a top-level loop is never a promoted prototype. What was
  added is a separate `N OSR loop windows` field on the same line; the misleading `0 of 0` is still
  what a reader sees first. Separately, `measure.sh`'s `[tier-1 decline]` section now prints an empty
  list for all four benchmarks: it greps for a fixed set of op names, and nothing declines any more,
  so it can no longer distinguish "nothing declined" from "the grep missed it".

Two smaller notes for whoever reproduces this:

- **Rebuilding `7e7d038db` today does not reproduce its own recorded startup numbers** — `run`
  (cached) measures 2,313,690 here against the 2,270,873 in that report (+1.9%), and `--version`
  2,293,407 against 2,250,406. Same commit, same pinned toolchain. Only an in-session A/B is worth
  quoting.
- **`scripts/perf-ratchet.sh` run directly reports CANNOT MEASURE on this box**, because it
  fingerprints bare `rustc -V` — now 1.97.1 — while the baseline holds the CI pin's 1.97.0.
  `gate.sh` passes `NOETA_PERF_TOOLCHAIN=1.97.0` and gets a clean run; a hand invocation must too.
- A `--no-default-features` release build emits **7 dead-code warnings** (the AOT archive helpers in
  `noeta-cli`), against the workspace's zero-warning rule. Default features are clean.

## Known costs and open items

- **The map read-modify-write cliff above.** Standing, not new, and the largest thing in this file by
  a factor of a hundred: a 240× slowdown and an O(entries) per-iteration copy, reachable by writing
  the key inline. Everything else here is percent.
- Top-level list-index loops are **+2.41%** vs pre-arc, still unattributed, down from +4.36%.
- `map_update_key` builds an owned key on every `set`, including pure overwrites — 23% of wordcount.
- The per-iteration key string is ~48% of assoc. Nothing in the map lane addresses it.
- `regex_syntax` remains the largest single relocation owner (11.50%); the `unicode` trade-off is
  unchanged and still a language-semantics decision, deliberately not taken.
- Startup has resumed creeping, ~1.4% per hundred commits, inside the ratchet's band so far.

## History

| date | commit | milestone measured |
|---|---|---|
| 2026-08-16 | `57091de12` | this report — re-measurement, no optimization landed: hot paths flat within 2.2%, maps still 2.3–3.0× PHP, tier 1 confirmed running on every row by four instruments, and a 240× map read-modify-write cliff found |
| 2026-08-02 | `7e7d038db` | the fix arc: tier-1 reaches the map/string loops, startup −41%, a directory-check quadratic removed, perf ratchet added |
| 2026-08-01 | `14e0707f1` | v0.4.0, 1,773 commits on: JIT flat, tier-0 +7–11%, startup/lowering regressed |
| 2026-07-17 | `8cda9ee7` | post audit-fix + aether merge; assoc root-caused + fixed |
| 2026-07-10 | `362b4873` | post PM/MCP/OTEL/p2p platform arcs (NOTE: off-lineage commit) |
| 2026-07-05 | `8e55b2f` | P-JSSA JIT milestone + map/string (SSO) cluster |

---

# Previous report (2026-08-02)

Date: 2026-08-02, main `7e7d038db` — the **fix arc** for the regressions the 08-01 run found:
eleven branches merged across three waves. Same machine, same competitor versions as every run
since 2026-07-05 (PHP 8.5.8, LuaJIT 2.1, Lua 5.5.0, Python 3.14.6).

**Method.** The wall-clock table was taken on a quiet box (3 pinned field runs, min-of-9,
min(total) − min(startup)). Everything *diagnostic* here is instructions retired, because the box
carried sibling agent builds for most of the arc and whole wall-clock field runs inflate ~2× under
that load while instruction counts hold to ~0.03%. Reproduce with
`scratch-bench/xlang/{measure.sh,xrun3.py}`; `scripts/perf-ratchet.sh` gates the same counts, and
`scripts/reloc-attribution.py` answers where load-time relocations come from.

## Startup (empty program, ms)

| Noeta-JIT | Noeta-int | JIT@v040 | JIT@0717 | PHP | PHP+JIT | Python | Lua | LuaJIT |
|----------:|----------:|---------:|---------:|----:|--------:|-------:|----:|-------:|
| **1.7** | 1.6 | 1.9 | 1.7 | 17.6 | 19.6 | 11.8 | 0.8 | 0.9 |

The four-run creep (1.2 → 1.7 → 1.9 → 2.2 ms) is arrested and reversed. Instruction counts resolve
what 0.1 ms of wall-clock cannot — see the table below.

## Compute time (ms) — lower is better

| bench | Noeta-JIT | Noeta-int | PHP | PHP+JIT | LuaJIT | Lua | Python |
|---|---:|---:|---:|---:|---:|---:|---:|
| loop 10M | 37.3 | 384.3 | 50.6 | 14.5 | **9.6** | 73.2 | 667.3 |
| fib(32) | 35.0 | 344.0 | 50.8 | 23.8 | **13.0** | 66.1 | 130.2 |
| strcat 50k | 3.7 | **3.4** | 11.3 | 10.9 | 32.4 | 36.6 | 15.9 |
| assoc 100k | 20.0 | 21.8 | 9.3 | **8.2** | 19.0 | 37.9 | 42.9 |
| wordcount 200k | 22.8 | 27.0 | 7.9 | 6.3 | **5.5** | 13.3 | 40.0 |

**Standings unchanged, position improved.** Best startup among the JIT-having languages (10× PHP,
7× Python); wins string building outright (3.0× PHP, 4.3× CPython, 8.8× LuaJIT); clear of every
non-tracing engine on loops and calls (1.36× and 1.45× over the PHP baseline, 17.9× and 3.7× over
CPython), with LuaJIT still the ceiling. Dicts remain the weak spot but are **no longer an outlier**:
assoc is now within **1.05× of LuaJIT** (20.0 vs 19.0) and 2.2× of the PHP baseline, where it trailed
by 4.5× before this arc.

Against the 2026-07-17 binaries re-run in the same session: loop **−4%**, assoc **−13%**, wordcount
**−10%**, fib flat. `strcat` reads +14% there, but it is a 3.7 ms benchmark against a 1.7 ms startup
subtraction — its instruction count is ~29% *below* the 07-17 binary, which is the number to trust.

**And the tier-1 check passes on every row now** — assoc and wordcount show the JIT 1.09× and 1.18×
ahead of the interpreter. Before this arc those two columns were identical to three digits, which was
the tell that the JIT was not running on them at all.

## What the arc bought

Instructions retired, `95f14eeef` → `7e7d038db`:

| | before | after | Δ |
|---|---:|---:|---:|
| `run` (cached) | 3,841,082 | 2,270,873 | **−40.9%** |
| `run --no-cache` | 5,535,644 | 3,132,418 | **−43.4%** |
| `check` | 3,348,994 | 2,890,640 | −13.7% |
| `--version` | 2,508,381 | 2,250,406 | −10.3% |
| assoc | 517,394,821 | 332,210,682 | **−35.8%** |
| wordcount | 719,543,729 | 477,497,027 | **−33.6%** |
| strcat | 98,849,578 | 69,919,224 | −29.3% |
| `arith` (ratchet, interpreter) | 611,410,827 | 563,711,438 | −7.8% |
| `check`, 384-sibling directory | 180,389,372 | 85,910,230 | **−52.4%** |

That last row is a **fixed quadratic**, not a constant-factor win: `source_text_tiers` rebuilt the
whole workspace `PackageMap` once per source, so checking n files did n rebuilds of an n-entry map.
Per-sibling cost now falls with directory size instead of rising.

### Wave 1 — the regressions
1. **`set_reg` inlines again** (`3786e6ac8`). Byte-identical source to its 07-17 form; `dispatch_inner`
   had simply grown past LLVM's inlining budget.
2. **The string/map loops sustain tier 1** (`cf9a2cb7a`). `--jit-stats` said `0 of 0 prototypes
   native`: tier 1 declines any loop holding a non-native op, so those benchmarks were **our
   interpreter against PHP's JIT**. Five ops joined the *existing* shared leaf-op seam.
3. **A leaf is never registered with the cycle collector** (`fcc9b1488`). One predicate, the answer
   recorded in `ObjHeader::registered`. Folding the two lists **uncovered a latent bug** — the release
   path's copy omitted four payload types that own children, so cycles through them were never
   collected. Plus a word-at-a-time FxHasher, which exposed the Fx round as too weak at fewer rounds
   (worst bucket 64 → 20, empty buckets 198 → 0).
4. **Native declarations resolve on demand** (`dd794ba26`). `extend_reflection` materialized the whole
   registry into **every compiled artifact**, then deduplicated it quadratically — 1,830,075 of the
   2.19M "lowering" cost, which was not lowering at all (~10k).

### Wave 2 — the front end and the dispatch loop
5. **Cold opcode arms outlined** (`fbefb92ba`). `dispatch_inner` 79,589 → 34,540 bytes, spill slots
   3,432 → 1,560. `arith` −8.87%.
6. **The clap tree is built once** (`f181a1882`) and a flagless `noeta run <file>` never builds it.
7. **The grammar is built once per parse** (`fc6e6a7a0`). Parsing `echo 0` was constructing the type
   grammar **80 times** — counted, not estimated.

### Wave 3 — the structural items
8. **The embedded corpus costs two relocations** (`258ed0e14`), not one per file. Every conformance
   case added had been making every `noeta run` slower.
9. **A back-edge promotion compiles the loop's window** (`3891cb9d7`), not the whole prototype —
   region-scoped OSR. Also fixed a **blind instrument**: `--jit-stats` counted only whole-prototype
   natives, so a top-level-loop program reported `0 of 0 native` *while running natively*.
10. **One hole grammar per parse, and a workspace package map built once** (`45b7c187d`).
11. **`scripts/reloc-attribution.py`** (`7e7d038db`) — the instrument that rejected the binary split.

## Measured and rejected

Each of these was built and measured, not argued about:

- **Non-PIE.** Recovers the whole process-init regression (`--version` −27.95%, relocations 53,121 →
  251) and costs only ASLR-for-own-code — BIND_NOW, full RELRO, NX and PIC objects all survive.
  Rejected: Noeta runs third-party registry code, JITs executable memory, and carries `unsafe` in the
  VM. (Implementation note if revisited: it cannot live in `.cargo/config.toml` — both spellings also
  hit proc-macro units, which are `-shared` dylibs the flag breaks.)
- **DT_RELR relocation packing, now settled with a mechanism.** Collapsed all 50,053 `RELA` entries
  into a 13,728-byte bitmap and deleted the entire 1.2 MB `.rela.dyn`, for **0.23%**. So the ~12
  instructions per relocation are the *application* — load addend, add base, store — not the table
  read. **The only lever is emitting fewer relocatable pointers into static data.**
- **Splitting lsp/dap/mcp into separate binaries.** Measured **−3.29%**, against a priced cost of
  137.5 MB vs 51.7 MB (2.66× disk, 2.69× download, per platform × 4 targets, engine duplicated four
  times). Two premises died: `noeta_mcp` was no longer 8.9% (the corpus blob had already taken it to
  3.78%), and the TLS/HTTP stack is not dev tooling — it enters via `std.http` and a `noeta run`
  importing it needs all 23.4% of it.
- **Caching a chumsky grammar across parses.** Impossible: a parser is parameterized by the *input's*
  lifetime and `Boxed` is an `Rc`. Per-parse sharing was the lever.
- **Caching the hash in heap strings** (`zend_string`-style). assoc gains **exactly 0%** — it builds a
  fresh string per iteration and hashes it once. PHP's cached `h` pays because PHP *reuses* the object.

## Premises that measurement destroyed

Several of these were asserted confidently by the coordinator, and each was wrong:

- "JIT-native map ops are blocked on a layout-stable heap representation" — three reports old, false.
- "The map gap means our maps are slow" — no: the JIT never ran. **If the JIT and interpreter columns
  are equal, that is not "the JIT does not help", it is "the JIT is not running."**
- "The 18× is in lowering" — lowering was ~10k of 2.19M.
- "The check regression is eager prelude seeding" — 71k of 838k.
- "Gate the TLS stack out of the CLI" — worth ~3%, and it is not gateable anyway (`std.http`).
- "A giant dispatch function taxes every arm through shared register pressure" — outlining left `loop`
  and `fib` flat. **What a fat arm taxes most is itself.**
- "`PackageMap::set` is 15% of a directory check" — a 49-sample profile; it is 3.8%. The scaling test
  behind it found the real quadratic.
- "`noeta-ide` is an editor crate the CLI should not depend on" — it is the shared analysis engine;
  `noeta-lsp`/`noeta-mcp` are thin adapters over it. The name misleads, the layering is right.

## Known costs and open items

- **Top-level list-index loops are +4.5% vs pre-arc**, down from +9.6%. Region scoping recovered
  everything attributable to the `reachable_pcs` mechanism (it lands at parity with a control that
  strips `Stringify` from the leaf set); the residual belongs to the arc's other ops and is
  unattributed.
- A hot **function** whose loop is long still runs on the whole-prototype body — no regression, no
  gain. Closing it needs outlining.
- `workspace_editions` has the same unmemoized shape `workspace_packages` had; not quadratic today
  only because it is called once per link.
- `regex_syntax` is the largest single relocation owner (11.84%). Dropping `unicode` measures −3.02%
  — within 6k instructions of the entire rejected binary split — but removes `\p{...}` and Unicode
  case folding from `std.regex`. A language-semantics decision, deliberately not taken.

## The gate that should have caught all of this

~1,800 commits landed a 2× startup regression and a 7–11% interpreter regression and nothing noticed.
`scripts/perf-ratchet.sh` now pins instructions retired for five rows **plus the tier-1 engine each
row is expected to run on**, so a row that starts or stops reaching native code is a finding rather
than a number. It earned itself immediately: it refused to re-record a row whose engine had changed,
forced that row's tolerance to move with its engine, and caught a deterministic +1.03% interpreter
regression the agents had not reported (since repaid — `arith` is now 7.8% below the pre-arc floor).

## History

| date | commit | milestone measured |
|---|---|---|
| 2026-08-02 | `7e7d038db` | this report — the fix arc: tier-1 reaches the map/string loops, startup −41%, a directory-check quadratic removed, perf ratchet added |
| 2026-08-01 | `14e0707f1` | v0.4.0, 1,773 commits on: JIT flat, tier-0 +7–11%, startup/lowering regressed |
| 2026-07-17 | `8cda9ee7` | post audit-fix + aether merge; assoc root-caused + fixed |
| 2026-07-10 | `362b4873` | post PM/MCP/OTEL/p2p platform arcs (NOTE: off-lineage commit) |
| 2026-07-05 | `8e55b2f` | P-JSSA JIT milestone + map/string (SSO) cluster |

---

# Previous report (2026-08-01)


Date: 2026-08-01, main `14e0707f1` (v0.4.0 — **1,773 commits** since the last run: native
extensibility S1–S4, reflection unification, named arguments, qualified references, editions,
tier providers, AOT/`--native`, HMR, isolates/cancellation, para/* extraction). Same machine,
same competitor versions as every run since 2026-07-05 (PHP 8.5.8, LuaJIT 2.1, Lua 5.5.0,
Python 3.14.6), so the cross-language columns are directly comparable to the tables below.

**Method change, and why.** The machine was **not quiet** for this run — a sibling agent held a
release build through most of it (load 6–13), and wall-clock proved unusable: whole field runs
inflated 2× together. So the regression verdict here rests on **instructions retired** (`perf
stat -e instructions:u`, min-of-3, pinned to CPU 3), measured **against the actual 2026-07-17
binaries re-run in the same session** — they were still in `~/.cache/noeta-bench/`. A hardware
count of work done by this process is nearly immune to CPU contention, where wall-clock is not.
The wall-clock table is still reported (it agrees on direction) but it is the weaker instrument
this time. `xrun3.py` (A/B harness) and `icount.py` (instruction counts) reproduce both.

## Verdict

**The JIT did not regress. The interpreter and the startup path did.**

| bench | JIT now | JIT@0717 | Δ | int now | int@0717 | Δ |
|---|---:|---:|---:|---:|---:|---:|
| loop 10M | 1169.0M | 1168.0M | **+0.1%** | 13143.8M | 12282.0M | +7.0% |
| fib(32) | 946.8M | 939.6M | **+0.8%** | 9987.6M | 9165.9M | +9.0% |
| strcat 50k | 102.0M | 91.8M | +11.1% | 101.3M | 90.3M | +12.1% |
| assoc 100k | 543.6M | 493.0M | +10.3% | 542.4M | 488.6M | +11.0% |
| wordcount 200k | 747.9M | 687.0M | +8.9% | 748.2M | 683.6M | +9.4% |

(instructions retired, min of 3.) Hot JIT-compiled code is **flat** — 1,773 commits of language
and platform work landed on the tier-1 path with no measurable cost. `strcat`/`assoc`/`wordcount`
regress identically in *both* columns because their heap ops bail to the interpreter every
iteration: there is one tier-0 regression here, not two.

## Startup (empty program, ms)

| Noeta-JIT | Noeta-int | JIT@0717 | int@0717 | PHP | PHP+JIT | Python | Lua | LuaJIT |
|----------:|----------:|---------:|---------:|----:|--------:|-------:|----:|-------:|
| **2.2** | 1.9 | 1.8 | 1.6 | 19.4 | 22.9 | 13.1 | 0.8 | 0.9 |

Still best-in-class among the JIT-having languages and ~9× faster than PHP/Python — but the creep
is now a trend, not noise: **1.2 → 1.7 → 1.9 → 2.2 ms** across four runs. The binary grew
42.3 → 51.9 MB. See the startup decomposition below; this is the finding that matters most.

## Compute time (ms) — lower is better

| bench | Noeta-JIT | Noeta-int | PHP | PHP+JIT | LuaJIT | Lua | Python |
|---|---:|---:|---:|---:|---:|---:|---:|
| loop 10M | 39.9 | 452.5 | 52.7 | 11.1 | **9.9** | 75.0 | 690.5 |
| fib(32) | 35.6 | 408.9 | 51.6 | 22.5 | **13.1** | 67.2 | 135.1 |
| strcat 50k | 4.3 | **4.0** | 10.2 | 9.3 | 33.7 | 36.6 | 16.0 |
| assoc 100k | 26.9 | 29.2 | 7.7 | **6.0** | 19.3 | 40.7 | 44.8 |
| wordcount 200k | 29.2 | 29.7 | 6.7 | **3.7** | 5.4 | 13.9 | 43.2 |

Competitor columns reproduce the 2026-07-17 table closely (PHP 52.7/51.6/10.2/7.7/6.7 against
53.0/51.4/10.8/9.1/7.2 then), which is the check that the field is sound even on a busy machine.
**Standings are unchanged:** Noeta wins string building outright, beats the PHP baseline on loops
(1.3×) and calls (1.4×) and CPython by 17×/3.8×, trails the tracing JITs on both, and still trails
PHP on dicts. Nothing changed rank.

## The startup regression, decomposed

Instructions retired, interpreter binaries, **with the program alone in its own directory** (this
control matters — see the sibling-linking note):

| phase | v0.4.0 | @07-17 | factor |
|---|---:|---:|---:|
| process init (`--version`) | 2.463M | 1.833M | 1.34× |
| + parse | +707k | +489k | 1.45× |
| + check | +846k | +391k | 2.2× |
| + lower & run | **+2.04M** | **+112k** | **18×** |
| `run --no-cache` total | 5.348M | 2.336M | 2.3× |
| `run` (bytecode cache warm) | 3.686M | 2.030M | 1.8× |

Three separate things, in descending order of size:

1. **Lowering + VM boot: 112k → 2.04M instructions for an empty program.** This is the largest
   single regression in the suite and it is pure fixed cost — it is paid by every `noeta run`,
   every `noeta test`, every script. Profile attributes it to registry/tier seeding walked per
   invocation (`noeta_check::tiers::ext_fn_record`, `StdTierRunners::modules`,
   `Registry::resolve_fielded`) plus filesystem project discovery.
2. **Process init: +630k before `main` does any Noeta work.** Relocations grew 36,419 → 48,548 and
   `.text` 29.0 → 37.5 MB. This is the binary-size creep flagged in the last two reports arriving
   as a measurable cost. (`aws-lc`'s `OPENSSL_cpuid_setup` runs at every start via
   reqwest→rustls — ~4.6% of a cold run — but it is present in **both** binaries, so it is a
   standing cost, not a regression. Worth a lazy-init pass regardless.)
3. **Check: 2.2×.** The eager prelude seeding got more to seed. At 2026-07-17 the registry
   declared **zero** native enums/classes/structs/directives/traits; it now declares 11/5/12/2/7,
   with `ExtType` 31 → 44 and `ExtFn` 268 → 397. `register_prelude` materializes all of it into the
   checker's symbol tables — with a `String`/`HashSet` per declaration — before a one-line program
   is looked at. The architecture is right (a native class must be indistinguishable from a `.noe`
   class); the *timing* is what costs. Deferring materialization to the first lookup miss, with the
   registry staying the source of truth, keeps the property and skips the work a program never uses.
   Note memoizing per process would **not** help a one-shot CLI run — the work already happens once.
   Laziness is the lever, not caching.

**Sibling project linking is now O(directory), and this is new.** `noeta check empty.noe` in a
directory holding 7 `.noe` files costs **8.67M** instructions against **3.31M** for the same file
alone; the 2026-07-17 binary costs 2.22M either way. The entry's siblings are linked as its
project, so dropping a script beside others makes it pay for all of them. Presumably deliberate
(module namespaces), but it is a real cost on the "run a script" path and was not true before.

## The tier-0 regression

`noeta dump` output for `loop.noe` is **byte-identical** between the two binaries — the same
opcodes execute, so this is cost *per op*, not more ops: +862M instructions over 10M iterations,
about **+86 per iteration**. The profile shape barely moves (`dispatch_inner` 55–57%,
`apply_binary` 22–29%, `heap::release` 11–16% on both), i.e. the whole loop got uniformly more
expensive rather than one arm going bad. `dispatch_inner` is now **75,067 bytes of machine code**
(was 71,294) carrying 104 opcodes (was 90) — one function large enough that register pressure and
spill traffic scale with it.

One concrete piece was found and fixed (`3786e6ac8`): **`set_reg` had stopped inlining.** Four
instructions behind ~125 call sites, byte-identical source to its 07-17 form, appearing as a
3.04% *call* in the `loop` profile. `#[inline(always)]` removes it from the profile entirely and
recovers 0.2–1.1%:

| bench | fix | v0.4.0 | Δ |
|---|---:|---:|---:|
| loop 10M | 13,063,781,019 | 13,143,780,659 | −0.6% |
| fib(32) | 9,962,889,334 | 9,987,561,028 | −0.2% |
| strcat 50k | 100,462,580 | 101,262,418 | −0.8% |
| assoc 100k | 536,353,186 | 542,451,255 | −1.1% |
| wordcount | 739,968,918 | 748,170,904 | −1.1% |

That is about a tenth of the gap. **The remaining ~6–9% is diffuse codegen pressure in one 75 KB
function**, and the lever that fits it is outlining: keep the hot arms in the dispatch loop and
push cold/rare opcodes behind `#[inline(never)]` so the hot arms' register allocation improves.
The file already documents this class of problem one screen from `set_reg` — the shared leaf-op
helpers carry `#[inline(always)]` because "the dispatch loop is a huge inlining site a plain hint
loses". Nothing here argues for removing opcodes or reverting language work; it argues for telling
the compiler which arms are cold.

## What to do, in value order

1. **Lowering/VM-boot fixed cost (112k → 2.04M).** Biggest single win, affects every invocation.
   Seed tier/registry records lazily rather than per boot.
2. **Lazy prelude materialization in the checker** (2.2×, and it grows with every native type
   added — this one gets worse on its own).
3. **Outline cold opcodes from `dispatch_inner`** to recover the remaining tier-0 6–9%.
4. **Binary size / relocations** (+630k per start). Feature-gating the TLS stack out of the core
   run path is the obvious candidate; `aws-lc` lazy init helps both this and the standing cost.
5. JIT-native map ops — still the standing item from previous reports, unchanged and still the
   reason dicts trail PHP.

## History

| date | commit | milestone measured |
|---|---|---|
| 2026-08-01 | `14e0707f1` | this report — v0.4.0, 1,773 commits on. JIT flat; tier-0 +7–11%; startup/lowering regressed; `set_reg` inlining fixed |
| 2026-07-17 | `8cda9ee7` | post audit-fix + aether merge; assoc root-caused + fixed (teardown sort guard + MapKey box) |
| 2026-07-10 | `362b4873` | post PM/MCP/OTEL/p2p platform arcs (NOTE: off-lineage commit) |
| 2026-07-05 | `8e55b2f` | P-JSSA JIT milestone + map/string (SSO) cluster |

---

# Previous report (2026-07-17)


Date: 2026-07-17, main `4a185e21` (post architectural-audit fix arc — god-file splits, VM perf
cluster, checker interning, front-end/stdlib decoupling, crate renames — MERGED with the
noeta-aether framework arc). Same machine, same competitor versions, same method (3 pinned field
runs, min-of-9 after warmup, min(total) − min(startup); run 1's PHP startup was inflated again, so
PHP columns use runs 2–3 per the documented precedent).

## Startup (empty program, ms)

| Noeta-JIT | Noeta-int | PHP | PHP+JIT | Python | Lua | LuaJIT |
|----------:|----------:|----:|--------:|-------:|----:|-------:|
| **1.9** | 1.3 | 17.4 | 19.1 | 11.9 | 0.9 | 0.9 |

Unchanged profile. The release binary SHRANK 55 → 42 MB across the two arcs (stdlib decoupling +
dead-weight sweeps) — the size creep flagged last run reversed.

## Compute time (ms) — lower is better

| bench | Noeta-JIT | Noeta-int | PHP | PHP+JIT | LuaJIT | Lua | Python |
|---|---:|---:|---:|---:|---:|---:|---:|
| loop 10M | 38.5 | 413.4 | 53.0 | 14.9 | **13.4** | 73.0 | 673.1 |
| fib(32) | 34.1 | 357.0 | 51.4 | 25.3 | **12.6** | 65.6 | 126.5 |
| strcat 50k | **2.8** | 2.9 | 10.8 | 10.9 | 32.6 | 36.9 | 11.5 |
| assoc 100k | 23.8 | 24.7 | 9.1 | **8.2** | 19.0 | 38.2 | 39.4 |
| wordcount 200k | 27.0 | 26.3 | 7.2 | 7.2 | **5.5** | 12.7 | 37.2 |

(loop_fn — the fn-local variant — was not re-measured this run.)

## vs 2026-07-10, and what the refactor arcs did

- **loop / fib / wordcount / startup: flat.** ~250 commits of deep restructuring (VM/checker/CLI
  splits, RunOptions consolidation, qualified extern identity, front-end decoupling) landed with
  zero regression on every hot path this suite exercises. strcat improved 3.2 → 2.8 ms.
- **assoc: regression found, root-caused with perf, FIXED — and then some (46 → 23.8 ms, 1.6×
  faster than the old lineage ever was).** The morning run showed +18% vs 07-10; four-binary A/B
  proved it predated both arcs (the 07-10 commit `362b4873` is not even an ancestor of today's
  main — its lineage diverged ~800 commits back). perf (installed mid-investigation) then showed
  the real story: (a) P-PKEY's inline Packed variant had grown `MapKey` 24 → 40 bytes — a
  footprint tax on every entry, fixed by boxing (`6bedab20`); (b) **~21–31% of the whole
  benchmark, on BOTH lineages, was `children()`'s deterministic destructor-order sort at map
  teardown — sorting 100k string keys on a map whose values are all immediates, i.e. zero
  children survive the filter.** Now guarded on actually holding a pointer value (`8cda9ee7`);
  maps with heap values keep the exact sorted walk. MapKey::cmp also re-inlines (#[inline] —
  the 4-variant match had stopped inlining into the sort, +13% instructions, perf-verified).
- Standing conclusions, updated: best-in-class startup and string building; loops/calls clear of
  every non-tracing engine; **dicts no longer an outlier** — assoc is within 1.24× of LuaJIT and
  2.8× of PHP+JIT (was 4.7–5.6×), ahead of Lua and CPython. JIT-native map ops remain the
  next lever.

## History

| date | commit | milestone measured |
|---|---|---|
| 2026-07-17 | `8cda9ee7` | this report — post audit-fix + aether merge; assoc root-caused + fixed (teardown sort guard + MapKey box) |
| 2026-07-10 | `362b4873` | post PM/MCP/OTEL/p2p platform arcs (NOTE: off-lineage commit) |
| 2026-07-05 | `8e55b2f` | P-JSSA JIT milestone + map/string (SSO) cluster |

---

# Previous report (2026-07-10)

Date: 2026-07-10, main `362b4873` (post MCP arc, PM Phase 3/4 + provenance, kernel methods,
p2p P3, OTEL metrics/logs, profiler UI — ~500 commits since the previous run). Machine: local
(CachyOS, x86-64), pinned to CPU 3. Interpreters: Noeta (release, JIT-on default + interpreter-only
`--no-default-features`), PHP 8.5.8 (baseline + opcache tracing JIT), LuaJIT 2.1, Lua 5.5.0,
Python 3.14.6 — same competitor versions as the 2026-07-05 run, so numbers are directly comparable.

**Method.** Same source algorithm in every language (uniform `while` loops, verified to print an
identical result). Each program run as an external process **pinned to one core**; **min wall-clock
over 9 reps** (after 1 warmup); 3 such full-field runs. Compute is isolated as
**min(total) − min(startup)** across the field runs — reconstructing raw totals first, because
subtracting *per-run* startup lets an inflated startup sample masquerade as deflated compute (this
bit run 1's PHP column). `xrun.py` reproduces a field run.

**New this run:** Noeta's default-on bytecode startup cache (landed post-2026-07-05) is active; the
warmup primes it, and the empty-program subtraction keeps compute honest. The interpreter column is
unchanged vs last run, confirming the cache isn't flattering the compute numbers.

## Startup (empty program, ms)

| Noeta-JIT | Noeta-int | PHP | PHP+JIT | Python | Lua | LuaJIT |
|----------:|----------:|----:|--------:|-------:|----:|-------:|
| **1.7** | 1.5 | 17.1 | 19.6 | 11.8 | 0.9 | 0.6 |

Still near-Lua, ~7–12× faster than Python/PHP. Slightly up from 1.2 ms on 2026-07-05 — the binary
grew 18 → 55 MB over the intervening arcs (PM, p2p, telemetry, profiler), worth watching.

## Compute time (ms, min-total − min-startup over 3 pinned field runs) — lower is better

| bench | Noeta-JIT | Noeta-int | PHP | PHP+JIT | LuaJIT | Lua | Python |
|---|---:|---:|---:|---:|---:|---:|---:|
| loop 10M — top-level (int `%`+`+`) | 39.0 | 403.8 | 52.3 | 14.8 | **13.8** | 72.7 | 663.5 |
| loop 10M — fn-local (same algo, local vars) | **18.9** | 419.5 | — | — | — | — | — |
| fib(32) (calls) | 34.0 | 353.7 | 51.7 | 24.5 | **13.1** | 66.6 | 129.7 |
| strcat 50k | **3.2** | 3.2 | 11.0 | 10.3 | 32.7 | 35.9 | 16.0 |
| assoc 100k (dict) | 38.8 | 38.5 | 9.0 | **8.2** | 19.6 | 37.4 | 43.4 |
| wordcount 200k (map) | 26.9 | 26.7 | 8.1 | 6.1 | **5.8** | 13.2 | 40.7 |

(±~15% run-to-run noise on a live machine; ranks are stable. All languages verified to print
identical output per benchmark, all three runs.)

## Change since 2026-07-05 (`8e55b2f`), same machine & competitors

| bench | JIT before | JIT after | Δ | interp before | interp after |
|---|---:|---:|---|---:|---:|
| loop (top-level) | 54 | **39.0** | **1.4× faster** | 410 | 404 |
| loop (fn-local) | 16 | 18.9 | ~flat (noise) | 330 | 420 † |
| fib(32) | 37.3 | **34.0** | ~9% faster | 401 | **354** |
| strcat 50k | 3.1 | 3.2 | flat | 3.1 | 3.2 |
| assoc 100k | 38.8 | 38.8 | flat | 39 | 38.5 |
| wordcount 200k | 26.6 | 26.9 | flat | 26.3 | 26.7 |

† single measurement each side, not a per-cell min-of-3-fields; within a live machine's noise band
for the interpreter.

The headline: the **top-level loop dropped 54 → 39 ms** — consistent in all three field runs, and
interpreter-flat, so it's a genuine JIT-side execution gain, not the new startup cache. Noeta now
**beats the PHP baseline outright on the top-level loop** (52.3 vs 39.0) where 2026-07-05 was
parity. `fib` gained ~9% (and 12% interpreted). The string/map cluster held exactly — no regression
despite ~500 commits of feature work across the VM (telemetry hooks, profiler hook, PM native ABI,
kernel methods all carried perf gates, and they held).

## Reading

- **Startup: Noeta wins** among the JIT-having languages (~1.7 ms). Best-in-class for CLIs /
  short scripts / serverless. (Small creep vs last run; binary size is the suspect.)
- **String building: Noeta wins the whole field** — 3.2 ms is 3.2× PHP+JIT, 5× CPython, 10× LuaJIT
  (compiled precise reference counting makes `s = s ~ "x"` an in-place O(n) extension).
- **Integer loop: now beats PHP-baseline** (1.3×) and is 17× CPython; gap to the tracing JITs
  narrowed to 2.6–2.8× (was 3.7–4.5×). The fn-local variant (18.9 ms) remains PHP-JIT-class —
  the residual top-level gap is still the global-slot round-trip.
- **Function calls (fib): ahead of PHP-baseline (1.5×) and CPython (3.8×)**, within 1.4× of
  PHP-JIT. LuaJIT (13.1 ms) is still the ceiling.
- **Dict / map: unchanged — the remaining gap.** assoc/wordcount hold at ~4.4–4.7× behind PHP.
  The map itself is not the bottleneck (raw Rust with the same hashbrown+FxHash map also loses to
  PHP's arena `zend_string`); the residual is diffuse per-iteration interpreter scaffolding.
  JIT-native map ops (blocked on a layout-stable heap representation) remain the lever.
- **LuaJIT is the ceiling** on loops and calls; **PHP 8.5** remains a very strong baseline.

**Bottom line:** Noeta holds its method-JIT-class profile and improved where it was weakest —
loops and calls — while ~500 commits of platform work (package manager, MCP, telemetry, p2p,
profiler) landed with **zero perf regression** on this suite. Best-in-class startup and string
building; loop/calls now clear of every non-tracing engine in the field; maps remain the one
workload where PHP leads, with a known lever queued.

Not covered here (Noeta's design strength): columnar `@packed` SoA + bulk vector kernels, where a
prior run beat PHP 1.7–4.4×. That's an apples-to-oranges data-parallel workload, worth a separate
showcase.


