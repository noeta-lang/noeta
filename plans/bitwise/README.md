# Bit-level computation arc — bitwise ops → fixed-width integers → packed types & SIMD

**Status: TIER B COMPLETE.** Tiers W and P remain (design below). Standard commit trailers.
This is a *new track*, independent of the in-progress memory-management Phase 5.

**Tier B shipped (on `main`), all slices green (conformance / differential 0-skipped / leak 0):**
- **B0 — integer literal forms** (`0x`/`0o`/`0b` + `_` separators) — already present from earlier work.
- **B1a — `& | ^ <<`** — new `Amp`/`Caret`/`Shl` tokens; `Pipe` reused for bitwise-OR in expression
  position; `BinaryOp::{BitAnd,BitOr,BitXor,Shl,Shr}`; **Rust-style precedence** (shifts just below
  additive, then `&`/`^`/`|`, all *above* comparison → no C footgun); integer-only, non-int operand →
  **E0043** (the doc's provisional "E0034" — actual next-free was E0043); shared `apply_binary` arm in
  both backends; shift amount outside `0..=63` panics deterministically (reuses E0010, not a new code).
  Folded the multiplicative + comparison pratt groups into single `choice` entries to stay under
  chumsky's 26-op tuple cap.
- **B1b — `>>`** — composed from two adjacent `Gt` in the expression pratt (never lexed as one token),
  so nested generic closes (`Map<K, List<V>>`, `x.as<List<int>>()`) stay in the disjoint type grammar.
  The hazard, resolved via option (1).
- **B2 — `!` complement** — `!` is operand-typed (bool→bool unchanged, int→int bitwise complement,
  `!x == -(x+1)`); no `~` clash (that stays concat).
- **B3 — shift domain** — folded into B1a (panic on out-of-range).
- **B4 — popcount intrinsics** — `count_ones`/`count_zeros`/`leading_zeros`/`trailing_zeros`/
  `rotate_left`/`rotate_right`/`reverse_bits`/`swap_bytes` as `int` methods; shared
  `lang_stdlib::IntMethod` + `int_method`; checker types them via `method_return`/`method_params`
  (all return `int`; `rotate_*` take one `int`). `count_zeros`/`leading_zeros` are width-relative to
  i64 (documented) — exact-width waits on Tier W.

## Why this exists

Today the language has **no bit-level computation of any kind**. The operator surface is
`+ - * / % ~ == != < <= > >= && ||` plus unary `- ! ...` and `?? ??=`; the only integer type
is `int` (signed i64); the `math` module is `sqrt/pow/abs/floor/ceil/round/min/max/pi/e`. There
are no `& | ^ << >>`, no bitwise complement, no `count_ones`/`leading_zeros`, no unsigned or
fixed-width types, and no SIMD. So the entire class of optimization in
<https://zed.dev/blog/zed-decoded-rope-optimizations-part-1> — packing chunk-presence into a
`u64` mask, `count_ones()` to turn a mask into an index, shift-addressing, SIMD newline scans —
**cannot be expressed at all**.

The type-system direction note already anticipates the tail of this arc: the inferred-static
type track (now complete) was explicitly described as **gating packed-types/SIMD**. This plan is
the concrete arc from "no bits" to "Zed-blog-class bit/SIMD code," staged so each tier is
independently shippable and the cheap, useful part can land long before the expensive part.

## The three tiers

| Tier | Delivers | Cost | Unlocks |
|---|---|---|---|
| **B — Bitwise operators on `int`** | `& \| ^ << >>`, complement, hex/bin literals, popcount-class intrinsics | Small (operators are `Op::Binary` discriminants; no value-repr change) | `int`-as-bitset for small flag work, mask plumbing |
| **W — Fixed-width integers** | `u8/u32/u64` (+ maybe `i8/i16/i32`), wrapping arithmetic, logical shift, typed conversions | Large (type-lattice + checker + value-repr decisions) | *Correct* masks (defined wraparound, no sign-extension), the substrate for SIMD lanes |
| **P — Packed types & SIMD** | `Simd<T, N>` lanes, elementwise ops, reductions + movemask, load/store | Milestone (new value kind; const-generic lane counts; scalar-semantics-first) | The Zed-blog class: SIMD scan/count, mask→index |

**Dependency:** B is standalone. W depends on B (its operators) and the completed inferred-static
type system. P depends on W (lane scalars) and likely **const generics** (`Simd<u8, 16>` needs a
const `N` — a prerequisite the S-track's bounded generics do *not* yet provide; see "Prerequisites").

## The oracle through-line (read this first — it shapes every slice)

The project's hard invariant is `--differential` at **0 skipped, backends agree by construction**:
the register VM and the IR tree-interpreter must produce identical `RunResult` on every compiled
program. Every slice below preserves this the same way:

- **Tier B** adds operators as new `BinaryOp`/`Op::Binary` discriminants resolved in the shared
  `lang-value/src/ops.rs` `apply_binary` (the VM and the eval leaf-executor both call it), so both
  backends get identical semantics for free — no new runtime op *shape*, nothing to skip.
- **Tier W** does **not** add new NaN-box tags (recommended): fixed-width values are physically the
  existing i64 word, and the *compiler* emits width-aware ops that mask the result to the declared
  width through a **single shared masking helper**. This mirrors how declared unions are *erased* (a
  union value IS its concrete value) — width lives in the type, not the runtime tag. Both backends
  share the helper ⇒ agreement by construction; the value model is unchanged.
- **Tier P** defines SIMD types with **scalar fallback semantics first** — every lane op is specified
  elementwise and implemented with a plain loop in both backends. The differential then holds at
  0-skipped because the semantics are portable and identical. *Real* SIMD codegen is a later
  **perf-only** swap behind byte-identical semantics (it cannot change `RunResult`, so it cannot break
  the oracle). This is the single most important design decision in the tier.

Net: nothing in this arc requires divergent backend behavior. Bitwise = more `apply_binary` arms;
fixed-width = erase-to-i64 + shared mask; SIMD = scalar semantics with an optional perf swap.

## Diagnostic codes

The doc's original E0034/E0035 allocation was stale — many codes shipped between writing this and
starting the track (object-model, packed-types, isolates took E0029–E0042). **Actual, as built/next:**

| Code | Meaning | Tier | Status |
|---|---|---|---|
| E0043 | Bitwise/shift operand is not an integer (`NonIntegerBitwise`) | B | ✅ shipped |
| (E0010) | Shift amount out of range — reuses the runtime `Panic` path, no new code | B | ✅ shipped |
| E0044+ | Fixed-width literal out of range / lossy conversion / mixed-width arithmetic | W | free |
| E00xx | SIMD lane-type / lane-count mismatch | P | free |

Next free is **E0044**. (Tier W/P codes to be finalized per slice against the then-current next-free.)

---

## Tier B — Bitwise & shift operators on `int`

The cheap, self-contained, genuinely-useful slice. `int` is signed i64; these operate on the full
64-bit value (the NaN-box immediate range is ±2⁴⁷, but `as_int`/`int` already box larger magnitudes
transparently, so a mask with the high bits set just boxes — no special handling). Each slice is its
own green commit.

### B0 — integer literal forms (do this first; masks are unreadable in decimal)
- **Lexer** (`lang-lexer`): accept `0x…` (hex), `0b…` (binary), `0o…` (octal), and `_` digit
  separators in all integer literals (`0xFF_FF`, `1_000_000`, `0b1010_0101`). Decimal unchanged.
- **Parser/const-eval:** fold to the same `int` value; out-of-i64-range literal is an error (reuse the
  existing literal-overflow path or E0035 later).
- Conformance: each radix parses to the expected value; `_` is positional-only.

### B1 — the binary operators `& | ^ << >>`
- **Lexer:** add `Amp` (`&`), `Caret` (`^`), `Shl` (`<<`). **Reuse `Pipe`** (`|`) for bitwise-or — it
  already exists for union types ("a single `|` only ever appears in type position" today); this
  extends it to *expression* position, which is unambiguous because the type and expression grammars
  are disjoint contexts. **`>>` is deferred to B1b** (see the hazard below).
- **AST:** `BinaryOp::{BitAnd, BitOr, BitXor, Shl, Shr}` + `symbol()` arms. Not overloadable in v1
  (`overload_method` → `None`); a `Bits` trait for user types is a later option, noted.
- **Precedence — DECISION POINT.** C's "bitwise below comparison" precedence is a notorious footgun
  (`a & b == c` parses as `a & (b == c)`). **Recommendation: follow Rust** — shifts bind just below
  the additive tier (`+ -`); `&`, then `^`, then `|` sit below the comparison operators; and the
  checker/linter can warn on mixing `&`/`|` with comparison without parens. Confirm exact tiers
  against the existing precedence ladder in `lang-parser` when implementing.
- **Checker** (`lang-check`): both operands must be `int` → result `int`; non-int operand → **E0034**.
  (Bool operands stay with `&&`/`||`; `&`/`|` are integer-only — do **not** silently accept bool.)
- **Both backends:** one arm each in `apply_binary` (`lang-value/src/ops.rs`), operating on `i64`.
  Shifts use `wrapping_shl`/`wrapping_shr` (or the domain check in B3). No new `Op` shape — the VM and
  eval already route `BinaryOp` through `apply_binary`, so this is the entire backend change.
- Conformance + differential: masks, set/clear/test-bit idioms; both backends agree.

### B1b — `>>` without breaking nested generics (THE hazard)
Nested generic type arguments close with `>>` today (`Map<K, List<V>>` lexes as two `Gt`). If `>>`
becomes a single `Shr` token, `List<int>>` mis-lexes and every nested generic breaks — the classic
C++ `>>` problem. **Do not lex `>>` as one token.** Two safe resolutions (pick one):
1. **Compose in the parser:** keep lexing two `Gt`; in *expression* position, the operator parser
   recognizes two adjacent `Gt` (no intervening trivia) as a right-shift. Type-argument parsing is
   unaffected (it consumes `Gt` one at a time).
2. **Split on demand:** lex `Shr`, and have the type-argument parser *split* a `Shr` back into two
   `Gt` when it needs to close generics (the Rust/Java/C# approach).
Recommendation: **(1)** — it keeps the type grammar byte-identical and localizes the change to the
expression operator table. Add a parser snapshot proving `Map<K, List<V>>` and `a >> b` both parse.

### B2 — bitwise complement via `!` (no `~` clash)
`~` is already string/list concat (`Tilde`, `TildeEq`); overloading it for complement is confusing.
**Recommendation: extend the existing unary `!` (`UnaryOp::Not`) to integer operands** = bitwise
complement, exactly as Rust does (`!x` is logical-not on bool, bitwise-not on integers). No new token,
no `~` ambiguity.
- **Checker:** `!bool → bool` (unchanged), `!int → int` (new). Any other operand → existing unary
  type error.
- **Both backends:** the `apply_unary` `Not` arm gains an int case (`!i` → `!i as i64`, i.e. `-(i+1)`).
- Conformance: `!0 == -1`, `!mask` clears/sets, De Morgan identities; differential.

### B3 — shift domain & semantics (fold into B1 or its own slice)
- **`>>` on a signed `int` is an arithmetic (sign-extending) shift.** A *logical* (zero-fill) shift is
  only well-defined on an unsigned type — it arrives in **Tier W**. Document this clearly; it is the
  main reason `int`-only bit work is "good enough for flags, wrong for packing."
- **Shift amount domain — DECISION POINT.** For `x << n` / `x >> n` with `n` outside `0..=63`:
  options are (a) **runtime panic** (deterministic, both backends; reuse E0010 or add E0034), (b)
  **mask the amount** `n & 63` (C/x86 hardware behavior, no error), (c) **defined-to-0/sign**. 
  Recommendation: **(a) panic on `n < 0 || n >= 64`** — it is the least-surprising checked semantics
  and trivially identical across backends. If `n` is a constant, the checker can raise **E0034**
  statically; a dynamic `n` panics at runtime.

### B4 — popcount-class intrinsics (the mask→index primitives)
The Zed rope turns a presence-mask into an index with `count_ones` over a masked-off low region;
`leading/trailing_zeros` find first/last set bit. Add these as **methods on `int`**:
`count_ones()`, `count_zeros()`, `leading_zeros()`, `trailing_zeros()`, `rotate_left(n)`,
`rotate_right(n)`, `reverse_bits()`, `swap_bytes()`.
- **stdlib** (`lang-stdlib`): a new `IntMethod` enum + `from_name`, mirroring `ListMethod`/`SetMethod`.
- **Checker** (`lang-check/src/stdlib.rs`): method signatures (`int.count_ones(): int`, etc.).
- **Both backends:** dispatch in the VM's and eval's method-call paths, delegating to the `i64`
  inherent methods. No new `Op` (it's a method call).
- **Caveat to document:** `leading_zeros`/`count_zeros` are **width-relative** — on i64 they count
  against 64 bits. They become exact for the user's intended width only with Tier W; note this so a
  `u8`-minded user is not surprised that `(1).leading_zeros()` is 63.
- Conformance + differential.

**Tier B gates (each slice):** `lang test` (conformance grows), `lang test --differential`
(0-skipped, agree), `cargo test --workspace`, clippy + fmt, miri over any boxed-large-int path (a
mask with bit 48+ set boxes — validate retain/release). Bench is optional for B (no asymptotic change;
it's expressiveness, not speed) — but add a `vm` microbench if a slice claims a speedup.

---

## Tier W — Fixed-width integer types

The layer that makes masks **correct**: defined wraparound, logical (zero-fill) right shift, no
sign-extension, exact-width popcount. This is a real type-system and value-representation expansion —
the expensive middle of the arc — so its decisions are called out explicitly and should be settled
*with the user* before W1.

### Decision points (settle before building)
1. **Which types?** Full `{i,u}{8,16,32,64}`, or a focused `u8 / u32 / u64` (+ keep `int` = i64)?
   The rope tricks need `u64` masks and `u8`/`u32` lanes. **Recommendation: start `u8, u32, u64`**;
   add the rest on demand. (`int` stays the ergonomic default signed type.)
2. **Subtyping.** Are fixed-width ints subtypes of `int`? **Recommendation: NO** — implicit
   widen/truncate is a footgun. They are distinct scalar types; movement between them and `int` is via
   **explicit conversions** (W4). Mixed-width arithmetic (`u8 + u32`) is **E0037** (cast required).
3. **Value representation — the crux.** **Recommendation: erase-to-i64 + type-directed masking** (the
   union-erasure philosophy): a fixed-width value is physically the existing i64 word; the compiler
   emits width-aware ops that mask the result to the declared width via a shared helper, so wraparound
   and logical shift are correct in *both* backends with **no new NaN-box tags and no value-model
   change**. Width/signedness travel in the type and reach the backend through the existing
   checker→backend channel (the S-track already threads resolved types to the compiler, e.g.
   `resolve_type_of_sites`). The alternative (new NaN tags per width, or a boxed width-carrying cell)
   costs scarce tag space or an allocation per scalar — reject unless masking proves insufficient.
4. **Overflow policy.** Fixed-width arithmetic is for bit work, where **wrapping is the expected
   default**. **Recommendation: wrapping by default**, with `checked_*` (→ `Option`), `saturating_*`,
   and `wrapping_*` methods for explicit intent. `int` (i64) keeps its current policy.

### Slices
- **W1 — lattice + literals.** Add the chosen scalar types to `Type` (suggest `Type::IntN { signed,
  bits }` so one variant covers all widths; update `is_numeric`, `from_ref` for the new builtin names
  `u8`/`u32`/`u64`, subtyping = invariant/no-widen). Typed literals `255u8`, `0xFFu32`, `1u64`;
  out-of-range literal → **E0035**. Checker literal inference: a fixed-width context coerces an
  untyped int literal to that width if in range.
- **W2 — value repr + the masking helper (both backends).** Implement the shared
  `mask_to_width(value, bits, signed)` in `lang-value`; the compiler emits width-tagged binary/unary
  ops (e.g. extend `Op::Binary` with an optional width, or a new `Op::BinaryWidth { op, bits, signed }`)
  resolved from the operand types. The VM and eval both apply the op then the mask. miri-validate (the
  value is still an immediate/boxed i64 — no new heap shape, but confirm large-`u64` boxing).
- **W3 — wrapping arithmetic.** `+ - * / %` on fixed-width wrap to the declared width; `/ %` by zero
  panics as today. Differential: wraparound identities (`255u8 + 1u8 == 0u8`).
- **W4 — conversions & casts.** Explicit, total, and visible: `u8.to_u32()` (widen, always safe),
  `u32.to_u8()` (truncate — wrapping, named so), `int.to_u8()`/`u8.to_int()`, `checked_to_u8(): u8?`.
  Decide the surface (`as`-cast vs methods); `as` is already a keyword (S6 narrowing) — reusing it for
  numeric casts is possible but overloads its meaning, so **recommend conversion *methods*** and keep
  `as` for type-narrowing. Implicit conversion → **E0036**.
- **W5 — logical shift + exact-width intrinsics.** Now `>>` on an unsigned width is **logical**
  (zero-fill); the popcount-class intrinsics (B4) become width-exact (`(1u8).leading_zeros() == 7`).
  Re-point the B4 methods to consult width.
- **W6 — (optional) `BitSet` stdlib type.** A growable bitset over `[u64]` with `set/clear/test/
  count/iter_set_bits`, the ergonomic consumer of the whole tier and a natural conformance demo.

**Tier W gates:** as Tier B, plus **bench** any slice that claims a speed/space win (e.g. a `u8`
buffer vs `int` list for memory), and miri over the masking + conversion paths. The checker work is
the bulk; lean on the existing inferred-static engine and add focused unit tests per new rule.

---

## Tier P — Packed types & SIMD

The milestone the type system was built to gate — the Zed-blog class proper (SIMD newline scan, lane
counts, mask→index). Scope deliberately, and **lead with scalar semantics** so the oracle never
breaks.

### Prerequisites (flag before starting)
- **Const generics.** `Simd<u8, 16>` needs a *const* lane count `N`. The S-track shipped bounded
  *type* generics (`<T: Comparable>`) but **not** const-generic value parameters. This is a real
  prerequisite — either a small preceding track ("const generic params `<const N: int>`") or a
  restricted fixed-set of SIMD widths hard-coded as distinct types to avoid const generics in v1.
  **Decision point.**
- **Tier W** (the lane scalars) must land first.

### Slices
- **P1 — the `Simd<T, N>` type + scalar-semantics value.** A new heap value kind: an aligned lane
  array. Both backends store it identically and operate **scalar-ly** (a plain loop over lanes), so
  semantics are portable and the differential agrees by construction. No real SIMD yet.
- **P2 — elementwise ops.** `+ - * & | ^ << >>`, comparisons producing a lane-mask. Defined
  elementwise over the Tier-W scalar semantics (including wraparound).
- **P3 — reductions, masks, movemask.** `any()`, `all()`, `count()`, and **`movemask(): u64`** (pack
  per-lane high bits into an integer) — the exact primitive that turns a SIMD compare into an index
  via `trailing_zeros()` (B4). This is what "find the first newline in 16 bytes" compiles to.
- **P4 — load/store.** Build a `Simd` from a slice of a `u8`/`u32` buffer (or string bytes) and write
  back. Bounds + tail-handling defined.
- **P5 — real SIMD codegen (PERF-ONLY, benched).** Swap the VM's scalar lane loops for actual
  platform SIMD *behind byte-identical semantics*. Because it cannot change `RunResult`, the
  differential and conformance are unchanged; this slice is gated purely on **measured speedup** (the
  "bench every gain" mandate). The IR-interpreter may stay scalar — it is the reference.

**Tier P gates:** conformance + differential (scalar semantics, 0-skipped, agree), miri over the new
lane value's heap accounting, and — for P5 — a criterion bench proving the SIMD win on a realistic
kernel (newline-scan / byte-count over a large buffer).

---

## Capstone (optional) — a rope, to prove the arc

The blog's subject is a *rope*. Once Tier P lands, a `Rope` stdlib type (chunked text with a `u64`
chunk-presence summary indexed via `count_ones`, and SIMD newline/char scanning) is the natural
end-to-end demonstration that every primitive composes. Not required by the arc, but the obvious
conformance showcase and the thing that proves the tiers were the right ones.

## Cross-cutting work (all tiers)
- **Docs:** `docs/resources/02-syntax.md` (operators, literal forms, fixed-width types), and the
  language spec for the *settled* overflow/shift/conversion semantics (these are observable, so they
  belong in the spec, not just the plan).
- **`plans/deferred.md`:** add a row pointing here so the arc is discoverable from the deferral index;
  strike it when Tier B starts.
- **Memory:** on starting, add a `bitwise-arc` topic file + MEMORY.md index line; record the settled
  decision-point answers (types chosen, repr, overflow policy) since those are non-obvious and durable.

## Suggested sequencing
1. **Tier B in full** (B0→B4) — small, high-value, unblocks all flag/mask work. Ship and stop here if
   that is all that is needed.
2. **Tier W** only when *correct* masks (packing, logical shift, unsigned) are actually required —
   settle the four decision points with the user first.
3. **Tier P** only when SIMD throughput is the goal, and only after the const-generic prerequisite is
   resolved. Land scalar-semantics first; treat real SIMD as a separate benched perf slice.
