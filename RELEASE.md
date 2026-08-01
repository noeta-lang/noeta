# Releasing

How a Noeta release is cut, and what it drags along with it. The toolchain release is mostly
automated — the work is knowing what the automation does for you, what it does **not**, and which
of the ecosystem's repos actually need anything afterwards.

Read [The one thing that will bite you](#the-one-thing-that-will-bite-you) before you verify
anything.

---

## 1. The toolchain

### What you do

```sh
# from a clean main, with the work already merged
./scripts/gate.sh --full                       # full CI parity — see CONTRIBUTING.md
$EDITOR Cargo.toml                             # [workspace.package] version = "X.Y.Z"
cargo metadata --format-version 1 >/dev/null   # refreshes Cargo.lock, no build
git add Cargo.toml Cargo.lock
git commit -m "chore(release): bump workspace version to X.Y.Z"
git push origin main
git tag -a vX.Y.Z -m "vX.Y.Z

<what ships, in a sentence or three>"
git push origin vX.Y.Z
```

The lock diff should be **version lines only** — one per workspace crate, nothing else. Anything
more means a dependency moved and you are shipping more than you think:

```sh
git diff Cargo.lock | grep -E '^[-+]' | grep -v '^[-+][-+]' | grep -v 'version = '
```

### What the tag does for you

Pushing `vX.Y.Z` triggers `.github/workflows/release.yml`, which runs these jobs:

| Job | What it does |
| --- | --- |
| `gate` | Re-runs the **entire** CI suite as the release gate — `ci.yml` reused, no drift. A tag push does not trigger `ci.yml` on its own, so without this a broken commit could ship binaries. |
| `build` | Four targets: `{x86_64,aarch64}-{unknown-linux-gnu,apple-darwin}`. |
| `package-extension` / `publish-extension` | The VS Code `.vsix`, published to the marketplaces. |
| `release` | The GitHub release: tarballs + `.vsix` + `SHA256SUMS`, with notes compiled from conventional commits. |
| `notify-web` | `repository_dispatch: release-published` → **noeta-docs, noeta-landing, noeta-playground**. |
| `advance-ecosystem-version` | **PATCHes the org-level `NOETA_VERSION` Actions variable to the tag.** |

Budget roughly **45–70 minutes**: the gate alone is ~45 min, then the four builds, then publish.

**Prerelease is detected from the tag.** Any `-` suffix (`v1.2.0-rc.1`, `v1.2.0-alpha`) marks it a
prerelease and **skips** `notify-web`, `advance-ecosystem-version` and the extension publish. An rc
must not move the fleet — that is deliberate.

Secrets it needs: `SITE_DISPATCH_PAT` (dispatch to the sites), `ORG_VARS_PAT` (a fine-grained PAT
with organization **Variables: write** — `GITHUB_TOKEN` cannot write org variables), `OVSX_PAT` and
`VSCE_PAT`.

### What you must NOT do by hand

- **Do not bump the org `NOETA_VERSION` variable.** The release does it. Setting it early points
  every para repo's CI at a release whose assets do not exist yet.
- **Do not go fix the docs site.** See below.

---

## 2. The web properties — they fix themselves

`noeta-docs` syncs `content/docs` from **the latest release tag**, not from `main`. So immediately
after a big push and before a release, the docs build can be **red on anchors that are already
correct in `main`** — it is building last release's content.

**Do not fix those anchors.** The `release-published` dispatch rebuilds docs against the new tag and
the failure disappears. To confirm ahead of time, build the docs locally against `main`:

```sh
cd ../noeta-docs && NOETA_DOCS_REF=main pnpm run prebuild && \
  NOETA_DOCS_REF=main pnpm exec astro build && pnpm run check:links
```

`noeta-registry` is **not** in the dispatch list — it carries no version pill and needs nothing.

`noeta-theme` has **no push CI at all**, by design: it is consumed via `file:../noeta-theme`, so its
gate is a PR workflow that builds all four consumer sites against the PR's theme. "No runs" is not
"broken" — do not go looking for a failure there.

---

## 3. The para packages

### First: do they need anything at all?

Usually **no**. Check before assuming, because the intuitive answer is wrong.

The four repos that ship native Rust — **para-api, para-db, para-html, para-p2p** — pin the
toolchain crates by git tag in their `crates/*/Cargo.toml`:

```toml
noeta-ext-abi = { git = "https://github.com/noeta-lang/noeta", tag = "vX.Y.Z" }
```

That pin looks like an ABI compatibility latch. **It mostly is not.** The composer generates a shim
carrying a `[patch]` that redirects *every* toolchain crate to the running binary's own source
(`crates/noeta-cli/src/compose.rs`), so a package pinned at an older tag composes against whatever
binary the consumer is running. Measured across a toolchain minor bump — including an
`ABI_VERSION` bump — published packages pinned to the *previous* tag ran unchanged under the new
binary, in both directions.

So a toolchain release does **not** oblige you to re-release the fleet. What the pin still governs
is whether each package's **Rust source** compiles against whatever it is patched to, which is a
real question with a real answer: build it (§5).

### What does force a change

**Each native repo's CI carries a guard** that fails when its pin ≠ `NOETA_VERSION`:

> `Cargo.toml pins toolchain vA but CI installs vB — bump the org variable NOETA_VERSION and the manifests together`

So once `advance-ecosystem-version` moves the variable, those four repos go red on that guard
alone. That needs a **pin-bump commit** in each. Whether you cut a *release* off it is a judgment
call: if the published version already works under the new toolchain, the version number buys
consumers nothing.

The four pure-Noeta repos — **para-aether, para-cli, para-aether-db, para-ai** — have no pins and
need nothing unless their sources actually break.

### When a package DOES need a release

Cut one when:

- its **source** stops compiling on the new toolchain (a real language change), or
- its **`toolchain = ">=X.Y"` floor** is now wrong, or
- it carries a committed **`noeta.lock` pinning a native dependency** you need it to stop using.
  Only packages with resolved dependencies have one; today that is **para-ai** (`para/api`) — the
  rest resolve fresh.

**A floor move is breaking.** If `toolchain = ">=0.3"` becomes `">=0.4"`, consumers on 0.3.x can no
longer build the package, so it is a **minor** bump (`0.2.x → 0.3.0`), not a patch. A patch release
that silently demands a newer toolchain is the thing a resolver cannot warn about usefully.

**A fleet minor bump invalidates every `^` requirement pointing at it.** Moving the fleet
`0.1.x → 0.2.0` breaks `{ version = "^0.1", package = "para/db" }` everywhere — in sibling
manifests **and in every example's manifest**. Sweep for them:

```sh
grep -rn 'version = "\^0\.1"' --include=noeta.toml ~/Code/para ~/Code/test-*
```

### Release order

Each package resolves its siblings **from the registry**, so a dependent cannot be verified — let
alone published — until what it depends on is live:

```
wave 1   para-api  para-aether  para-cli  para-db  para-html  para-p2p     (no para deps)
wave 2   para-aether-db                                  (needs para/aether + para/db)
wave 3   para-ai                                                    (needs para/api)
```

Per repo: bump `noeta.toml`, commit, merge to `main`, push, then `git tag -a vX.Y.Z` and push the
tag. Its `release.yml` re-runs CI as the gate and then `noeta publish`es to the registry.

**A pushed tag is not a published package.** Confirm each one is actually live before moving to the
next wave:

```sh
curl -fsS https://registry.noeta.dev/v1/packages/para/db | jq -r '.versions[].version'
```

---

## 4. After a large merge window, expect red — and diagnose before fixing

When many commits land before a release, most of the ecosystem goes red at once. **Almost all of it
is an ordering artifact of the single `NOETA_VERSION` pin**, not a defect: the repos' sources have
moved past the toolchain their CI still installs. Two signatures, both of which clear on their own
when the variable advances:

- `E0019 no module <x>` from `noeta check` — sources use language the pinned toolchain lacks;
- `clippy::needless_update` on `..ExtFn::DEFAULTS` — the pinned `ExtFn` has fewer fields than the
  source expects.

Fixing those by hand is wasted work. What is **not** an artifact, and has to be fixed: `cargo fmt`
drift, genuine ABI breakage in test code, stale manifest syntax, and anything that still fails after
the pin advances.

---

## 5. Verify by running the real thing

The toolchain's own CI being green tells you the toolchain is fine. It tells you nothing about
whether the packages still build. **Build the binary and run the actual commands** against the
actual packages — that is how the interesting failures surface, and there is no cheaper substitute:

```sh
CARGO_TARGET_DIR=~/.cache/noeta-verify cargo build --release -p noeta-cli
export PATH="$HOME/.cache/noeta-verify/release:$PATH"
noeta --version          # confirm it is the version you think it is
```

A target dir of your own keeps this out of the shared one (a workspace target here runs tens of
gigabytes, and `/tmp` is a 14G tmpfs).

The tag exists on the remote the moment you push it, so `tag = "vX.Y.Z"` resolves for cargo long
before the release workflow finishes. You can bump a native package's pin and build it immediately
rather than waiting.

Worth running per package: `noeta check` and `noeta test` on **every** root `.noe` **and every
example** (examples are where stale manifest syntax hides), plus `cargo clippy --all-targets` and
`cargo fmt --check` on each Rust crate.

### The one thing that will bite you

**Never pipe a command whose exit code is the verdict.**

```sh
noeta check f.noe | tail -2      # prints "0 error(s)" — and $? is tail's 0
```

`noeta check` can print a summary line and still exit non-zero (a resolve failure is not a check
error). Piping discards that. This produces a string of confident false greens and it is the single
most likely way to ship something broken while believing you verified it. Redirect and read `$?`:

```sh
noeta check f.noe >/tmp/out.log 2>&1; rc=$?
```

The same applies to `scripts/gate.sh`. And **a SKIP is not a PASS** — the gate lists skipped steps
in full for that reason.

### Other things that have actually gone wrong

- **A stale binary fails exactly like an unfixed bug.** If a build was interrupted, the binary at
  the expected path is the *old* one and your probe measures the old compiler. Check
  `noeta --version` against what you expect before trusting a result.
- **Read the diagnostic, not just the code.** An `E0058` on a reflection probe turned out to be
  about malformed construction syntax elsewhere in the file — the error code alone said "the fix
  does not work", the message said otherwise.
- **Fixtures inside strings are invisible to sweeps.** A `noeta.toml` written from a Rust
  `format!` in a test, or a manifest quoted in a `.md`, matches no `--include=noeta.toml` glob and
  survives every corpus migration. When a language change sweeps the corpus, grep the *content*
  too.
- **`git archive` reads `HEAD`.** Packaging examples for distribution before committing silently
  ships the old files.

---

## 6. Checklist

```
[ ] scripts/gate.sh --full green on main
[ ] version bumped, Cargo.lock diff is version lines only
[ ] committed, pushed, tag pushed
[ ] release workflow green — all jobs, not just the gate
[ ] GitHub release has 4 tarballs + .vsix + SHA256SUMS
[ ] org NOETA_VERSION reads the new tag
[ ] docs / landing / playground rebuilt green off the dispatch
[ ] para: native pins bumped where CI's guard demands it
[ ] para: any package needing a release identified — floor, source break, or stale lock
[ ] para: released in dependency order, each confirmed live in the registry
[ ] consumers (test-*) verified against the released binary, unpiped
```
