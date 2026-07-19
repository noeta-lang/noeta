# Design sketch — shared / attested watch ledger

**Status:** design note only (advisory-intake residual d). Not built. This records the options and a
recommendation so the residual can close to a decision rather than an open question.

## The problem with the local file

`noeta watch-scope` (advisory-intake tier 6) defends against a registry silently **suppressing** or
**rewriting** a scope's advisories over time. It does this by pinning a baseline — the advisory feed
head, the transparency-log checkpoint, and the set of advisory ids ever seen for the scope — in a small
local TOML file, then on each run checking the log only grew (append-only) and nothing previously seen
disappeared.

That state is **per machine**. Consequences:

- **First-run TOFU per watcher.** Each CI runner / developer establishes its own baseline against
  whatever the registry serves *at that moment*. A suppression that happened before a given watcher's
  first run is invisible to it — it simply never saw the advisory to miss it.
- **Ephemeral runners re-baseline silently.** A wiped/rebuilt CI runner loses the file and starts over,
  so the protection window resets on every fresh environment — precisely where CI lives.
- **No cross-watcher view, so no equivocation detection.** The subtlest registry attack is a *split
  view*: serve an honest history to most, a suppressed one to one target. Nothing today compares what
  different watchers saw, so a split view is undetectable.

Attesting/​sharing watch-state adds a **tamper-evident, shared memory** of the scope's advisory history:
a fresh watcher starts from the community high-water mark instead of re-TOFU'ing, and divergent
observations between watchers surface a split view.

## Options

1. **Signed state file committed to a repo.** The watch state (feed head, log checkpoint, seen ids),
   signed by the watcher's *keyless* identity, is committed to a git repo (the consuming project's, or a
   dedicated audit repo). *Pros:* trivial; reuses the keyless machinery already here; git history is the
   append-only log; a PR diff makes drift human-reviewable. *Cons:* only as shared as the repo; the
   state file is a merge-conflict magnet; no cross-org view; you trust whoever commits.

2. **A second transparency log.** Watchers publish their observed checkpoints to an append-only *witness*
   Merkle log (the registry already has the log machinery to reuse). *Pros:* real append-only +
   consistency proofs; anyone can audit; cross-checking observations detects equivocation. *Cons:* new
   hosting + a new signing role; who runs it; and the witness log's own trust must be bootstrapped.

3. **Witness cosigning (the CT / Sigsum / Go-checksum-db model).** Independent witnesses countersign the
   registry's checkpoints; a client trusts a checkpoint only if a quorum of witnesses cosigned the *same*
   tree head. *Pros:* the established, industry-proven anti-split-view mechanism; directly defeats
   equivocation; watchers become quorum-checkers rather than lonely TOFU-ers. *Cons:* needs a witness
   *ecosystem* — several independent operators — which is heavy for a young single-operator registry;
   plus a cosignature protocol and key distribution.

## Recommendation

**Phase it, and aim at cosigning.**

- **Near term — option 1 (committed signed state), opt-in.** Cheap, reuses keyless, and is a strict
  improvement over the per-machine file for a *team*: the baseline becomes shared, reviewable, and
  survives ephemeral runners. It also produces exactly the attested observation records the later
  options consume, so it is not throwaway.
- **Long term — option 3 (witness cosigning).** The threat `watch-scope` exists to counter — a registry
  serving a split view or suppressing to one consumer — is *exactly* what cosigning defeats, and it is
  the design the whole ecosystem has converged on (Certificate Transparency, Sigsum, the Go checksum
  database). Option 2 is subsumed by 3 (a witness log is just how cosigners publish), so it is not worth
  pursuing standalone.

**Trigger to build:** a second independent registry operator, or a consumer with a compliance need for
cross-party attestation. Until one of those exists there is no witness ecosystem for cosigning to stand
on, and the per-machine file plus the opt-in committed-state file (option 1) is sufficient.
