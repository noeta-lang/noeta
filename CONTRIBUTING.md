# Contributing

This is a pre-alpha, not-yet-public language implementation built primarily through agentic engineering. This file is the developer's entry point; it stays light and points at the deeper references rather than repeating them.

## Orientation

- **What the language is:** see `README.md` and the [wiki](docs/Home.md) (`docs/`).
- **How the implementation is structured:** see `ARCHITECTURE.md` (pipeline + crate map).
- **Conventions and the agent workflow:** see `AGENTS.md` — naming, formatting/linting, the new-feature template, and the testing discipline apply to humans and agents alike.
- **What to work on next:** see `plans/roadmap.md` and the per-slice files in `plans/m0/`.

## The loop

1. Pick the lowest-numbered `todo` slice in `plans/roadmap.md`; set it `in-progress`.
2. Implement it as a **vertical slice** through the pipeline (grammar/AST → eval op → conformance cases → snapshots). Prefer end-to-end feature slices over diffuse refactors.
3. **Every feature or fix lands with a conformance corpus entry** (`tests/conformance/**.lang` with `// expect:` headers). This is the iron rule.
4. Keep it green and clean:
   - `cargo test` (unit + snapshot + conformance + property)
   - `cargo fmt --all`
   - `cargo clippy --all-targets -- -D warnings`
   - `cargo build` with zero warnings
5. Review snapshot changes deliberately (`cargo insta review`) — never blind-accept.
6. Mark the slice `done`, update `roadmap.md`, and commit code + task file together.

## Conventions (summary; full list in `AGENTS.md`)

- Conventional-commit titles.
- American English in code, comments, and docs.
- No hard line wrap in markdown.
- Work on a branch / worktree to avoid conflicts with parallel agents.
- Each crate has a `README.md` (one paragraph: what it takes in, what it emits).
