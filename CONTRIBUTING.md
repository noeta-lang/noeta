# Contributing

This is a pre-alpha, not-yet-public language implementation built primarily through agentic engineering. This file is the developer's entry point; it stays light and points at the deeper references rather than repeating them.

## Orientation

- **What the language is:** see `README.md` and the [wiki](docs/Home.md) (`docs/`).
- **How the implementation is structured:** see `ARCHITECTURE.md` (pipeline + crate map).
- **Conventions and the agent workflow:** see `AGENTS.md` — naming, formatting/linting, the new-feature template, and the testing discipline apply to humans and agents alike.
- **What to work on next:** see `plans/roadmap.md` (the frontier) and `plans/backlog.md` (every open item, with source + trigger).

## The loop

1. Pick an *(active)* item from `plans/backlog.md` (or a backlog trigger that has fired); for multi-slice work, open a `plans/<arc>/` ledger.
2. Implement it as a **vertical slice** through the pipeline (grammar/AST → eval op → conformance cases → snapshots). Prefer end-to-end feature slices over diffuse refactors.
3. **Every feature or fix lands with a conformance corpus entry** (`tests/conformance/**.noe` with `// expect:` headers). This is the iron rule.
4. Keep it green and clean with `scripts/gate.sh`, which runs what `.github/workflows/ci.yml` runs and prints a per-step PASS/FAIL summary:
   - `scripts/gate.sh --quick` — `cargo fmt --all --check` + both `clippy -D warnings` splits (1m20s warm). Run it as you go.
   - `scripts/gate.sh` — the merge gate: adds the workspace suite, the lean-CLI and feature-shape builds, the doc samples, and the JIT oracles (~15 min warm, 35 min cold). **Run it, green, before merging to `main`.**
   - `scripts/gate.sh --full` — adds the wasm, miri, and editor-tooling jobs. Before a release tag.
5. Review snapshot changes deliberately (`cargo insta review`) — never blind-accept.
6. Strike the backlog row (or update the arc ledger; delete the arc directory when it ships) and commit code + task file together.

## Conventions (summary; full list in `AGENTS.md`)

- Conventional-commit titles.
- American English in code, comments, and docs.
- No hard line wrap in markdown.
- Work on a branch / worktree to avoid conflicts with parallel agents.
- `main` is pushed in batches, so GitHub Actions runs at push cadence rather than per merge. `scripts/gate.sh` runs the same gates locally, at the merge — treat a merge without it as one whose breakage you have deferred to whoever pushes next.
- Each crate has a `README.md` (one paragraph: what it takes in, what it emits).
