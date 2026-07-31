# Plans

This directory is the project's in-repo task tracker. It is plain markdown, version-controlled, offline, and agent-native — there is no external issue tracker for the language project itself.

## Why this exists

The language is built almost entirely by AI agents over a multi-year effort. Work must be decomposed into small, independently verifiable units and followed up on reliably. Tracking lives in the repo so that **a task file is updated in the same atomic commit as the code that advances it** — the task trail and the code never drift.

## Layout

- `roadmap.md` — where the project stands and what the good next picks are. Re-scan it at the start of every work session.
- `backlog.md` — the single registry of everything open: deferred items, scope cuts, and design proposals, each with its source and a concrete trigger. Scan it when planning a new pass so nothing slips.
- `backend-mirror.md` — the standing VM ↔ reference-interpreter mirror inventory & policy (which duplicated logic is irreducible vs liftable). A living reference, not a plan.
- `parallel-path-audit.md` — the standing inventory of logic maintained in N places with nothing forcing the copies together, ranked by evidence of drift, each row with a proposed chokepoint. Also records what was audited and found clean, so the next pass does not re-walk it. A living reference, not a plan.
- `<arc>/` — an active arc in flight gets its own directory: a `README.md` ledger (status header + slice table) and per-slice files as needed.

**Completed work is deleted, not archived.** When an arc ships, strike its backlog rows, move any new deferrals into `backlog.md` in the same commit, and delete its directory — the slice ledgers and design rationale stay available in git history. `plans/` only ever describes work that is open. (What the *product* does belongs in the wiki and `ARCHITECTURE.md`, never here.)

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
- [ ] Grammar / AST (noeta-ast, noeta-lexer, noeta-parser)
- [ ] Checker rule        (noeta-check)
- [ ] IR / bytecode       (noeta-ir, noeta-compiler)
- [ ] Both backends       (noeta-vm + noeta-eval, differential-covered)
- [ ] Conformance cases   (tests/conformance/...)
- [ ] Docs                (the relevant wiki page)

## Definition of done
<concrete, checkable>
```

## The iron rule

**Every feature or fix lands with a conformance corpus entry.** A change to behavior that does not add or update a `tests/conformance/**.noe` case is incomplete. This is what lets an agent verify a change end-to-end without human judgment.

## Working discipline

1. Pick an *(active)* backlog row (or a trigger that has fired) via `roadmap.md`.
2. For multi-slice work, open a `plans/<arc>/` directory with a ledger; for a small item, the backlog row is the tracker.
3. Implement as a vertical slice; add conformance cases; keep the differential oracle and leak gate green.
4. Run the tests green; `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` clean.
5. Update `backlog.md` (and the arc ledger) in the same commit as the code; on arc completion, delete the arc directory.
6. Work on a branch / worktree to avoid conflicts with parallel agents.
