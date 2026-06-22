# Plans

This directory is the project's in-repo task tracker. It is plain markdown, version-controlled, offline, and agent-native — there is no external issue tracker for the language project itself.

## Why this exists

The language is built almost entirely by AI agents over a multi-year effort. Work must be decomposed into small, independently verifiable units and followed up on reliably. Tracking lives in the repo so that **a task file is updated in the same atomic commit as the code that advances it** — the task trail and the code never drift.

## Layout

- `roadmap.md` — the milestone map (M0–M3) and the single source of "what's next / what's done". Re-scan it at the start of every work session to pick the next unit.
- `deferred.md` — the cross-milestone **deferral registry**: every non-gate item a done slice pushed to later, with its source slice and a concrete trigger. Scan it when planning a new pass so nothing slips between milestones.
- `m0/`, `m1/`, `m2/` — one file per work unit (a "slice"), grouped by milestone. Each slice is a vertical cut through the pipeline that is independently testable. M0, M1, and M2 cluster 1 (host IO & async foundation) are complete; later M2 clusters await their own planning passes.
- `done/` — completed slice files are moved here once their definition-of-done is met (and recorded in `roadmap.md`). Keep the trail; do not delete.

## Slice file shape

Each slice file carries a status header and follows the new-feature template:

```
# Slice NN — <title>

Status: todo | in-progress | done

## Goal
<one sentence>

## Scope
- In: ...
- Out: ...

## Checklist (vertical slice)
- [ ] Grammar / AST (lang-ast, lang-lexer, lang-parser)
- [ ] Checker rule        (n/a in M0 — no type checker yet)
- [ ] Bytecode            (n/a in M0 — tree-walker only)
- [ ] Eval op             (lang-eval)
- [ ] Conformance cases   (tests/conformance/...)
- [ ] Snapshots           (insta; reviewed, never blind-accepted)

## Definition of done
<concrete, checkable>
```

## The iron rule

**Every feature or fix lands with a conformance corpus entry.** A change to behavior that does not add or update a `tests/conformance/**.lang` case is incomplete. This is what lets an agent verify a change end-to-end without human judgment.

## Working discipline

1. Pick the lowest-numbered `todo` slice from `roadmap.md`.
2. Set its status to `in-progress`.
3. Implement it as a vertical slice; add conformance cases and snapshots.
4. Run `lang test` (or `cargo test`) green; `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` clean.
5. Mark the slice `done`, update `roadmap.md`, commit code + task file together (conventional-commit title).
6. Work on a branch / worktree to avoid conflicts with parallel agents.
