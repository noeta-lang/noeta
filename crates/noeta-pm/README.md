# noeta-pm

The Noeta **package manager** (package-manager milestone, Phase 2).

- **Takes in:** a `noeta.toml` manifest's `[dependencies]`.
- **Emits:** re-rooted source packages the loader links, a resolved dependency graph (pure PubGrub resolver), a reproducible `noeta.lock`, and the registry index client.

Everything a build needs to turn a manifest into linkable packages: manifest parsing (`manifest`), the transitive graph walk (`graph`), git fetch plus a content-addressed store, the lockfile (`lock`), the registry index (`registry`, with a git-forge backend as an alternative to the hosted index), and namespace reservation (`reserved`). This lives in a library — not the `noeta-cli` binary — so every front end (the CLI, `noeta-lsp`, and `noeta-db`'s salsa graph) resolves dependencies through the same code, and cross-package `use` resolves identically whether you run, check, or edit. It also carries feature-gated Phase 4/5 surface: `provenance` (Ed25519-signed release attestations + transparency-log verification + security-advisory intake and CVSS scoring) and `keyless` (Sigstore OIDC-identity signing/verification, offline-bundle-verifiable), each CLI-only so the LSP and other offline consumers never link the crypto/network stack. `fmt-config` optionally pulls `noeta-fmt` so `resolve_fmt_config` can read a manifest's `[fmt]` table through this crate's one manifest-discovery walk. `noeta-edition`'s `Edition` type is re-exported as `noeta_pm::edition`.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
