# Split the checker (`lang-check/src/lib.rs`, 5722 LOC)

Status: todo

The bidirectional type checker is one 5722-line file built around a single huge
`impl Checker`. The type *lattice* (`lang-types`) is clean; the debt is entirely
the checker file's size. This track splits the one `impl Checker` across
concern-scoped submodules **without touching behavior**.

## Goal

`lang-check/src/lib.rs` holds the `Checker` struct, `SiteMaps`, the shared result
types (`Checked`, `DestructorRelevance`), the `error()` helper, and the top-level
entry (`check_all` and `collect`); each cohesive checking concern moves to its own
`impl Checker` submodule. No file over ~1500 LOC; behavior byte-identical.

## Scope

- **In (submodules, each an `impl Checker { … }`):** the seams are already visible
  as method clusters (line numbers approximate, they drift):
  - `expr.rs` — expression synthesis/checking (`synth` ~3058 and its `synth_*`
    helpers, the biggest single cluster).
  - `decl.rs` — statement + declaration checking (`check_stmt` ~1334 and the
    record/class/enum/fn declaration checks).
  - `traits.rs` — trait/coherence (`check_trait_impl` ~2276, `check_coherence`
    ~2365, `check_derives` ~2404, `satisfies`/`builtin_satisfies`).
  - `attributes.rs` — directive/attribute validation (`check_attrs` ~2482,
    attribute construction, placement gate).
  - `packed.rs` — the packed/SIMD/width site collection and `IntN` arithmetic.
- **Out (stays in lib.rs):** the `Checker`/`SiteMaps`/`Checked` types, `collect`
  (pass 1, small and central), and `error()`.
- **Not this track:** the `BuiltinTrait` enum conversion (its own file,
  `builtin-trait-enum.md`) and any further `Checker`-field regrouping beyond the
  `SiteMaps` grouping already done.

## Design

The whole checker is one crate, so splitting an `impl Checker` across files needs
no visibility changes at all: each submodule declares `impl Checker { <moved
methods> }` and freely touches the struct's private fields and sibling private
methods (same-crate privacy). This is the cleanest of the four splits — pure
`mod` mechanics, no `pub(crate)` surface to design.

Recommended mechanics (per `state_machine.rs` precedent): for each cluster,
`sed`-extract the contiguous method range into `check/<name>.rs` wrapped in
`impl Checker { … }` with the right `use` header, add `mod <name>;` to lib.rs,
compile, fix the (usually zero) import gaps, run the differential, commit.

Some methods are interleaved rather than contiguous; move them in whatever order
keeps each commit compiling. Do **one submodule per commit**.

## Risks & constraints

- Low risk (same-crate, compiler-verified), high tedium (moving ~4000 LOC across
  files). The main hazard is a method that both clusters call — leave shared
  helpers in lib.rs or in whichever submodule owns them, imported by the other.
- The 189 checker unit tests pin rendered diagnostic output; they must stay green
  (they will, since nothing but code location changes).

## Checklist

- [ ] `expr.rs` (`synth` + `synth_*`)
- [ ] `decl.rs` (`check_stmt` + declaration checks)
- [ ] `traits.rs` (trait impl / coherence / derives / satisfies)
- [ ] `attributes.rs` (attribute/directive validation + construction)
- [ ] `packed.rs` (packed/SIMD/width sites + IntN arithmetic)
- [ ] each submodule a separate commit; 189 checker tests green throughout
- [ ] differential 417/0, backends agree; clippy `--all-targets` + fmt clean

## Definition of done

`lang-check/src/lib.rs` holds only the shared types + `collect` + `error()`, with
the five concern submodules each self-contained; the 189 checker tests and the
differential are unchanged and all gates green.
