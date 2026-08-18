---
type: Decision
title: Setup accepts a repo that's already a clean clone of dest, not just a completely empty one
description: setup's precondition relaxes from "no branches, no HEAD" to "every existing local branch's tip is identical to dest's own current tip for that branch name, and no other local branches exist" — recognizing `git clone <dest-url> && cd && gitprism setup` as a safe starting state instead of rejecting it outright, while still hard-failing on any real independent history.
tags: [architecture, setup, bootstrap]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

[decisions/0012](0012-config-versioned-in-source.md)'s Consequences state plainly:
"`gitprism setup` requires an existing, completely empty repository (no commits, no
branches) at or above where it's invoked... it refuses to run against a repo that
already has history." `src/commands/setup.rs`'s `has_existing_branches || repo.head().is_ok()`
check enforces exactly that, unconditionally.

That collides with what is arguably the *more* natural onboarding sequence: "I need
source to contain everything currently in dest, so I'll `git clone <dest-url> source`
first" — the standard git reflex for "start a new local repo from an existing one's
content" — before ever running `gitprism setup` at all. A plain `git clone` always
leaves at least one local branch checked out (the remote's default branch) with `HEAD`
attached to it, so today's guard rejects this sequence unconditionally, with no way
around it: there is currently no supported path from "I already have dest cloned" to
a working `gitprism setup` run.

The guard exists for a real reason, restated directly in 0012: `setup` must never
graft onto or silently check out over a repo that has *unrelated* real history — that
protection has to survive whatever changes here. The question is narrower: whether
"non-empty because it's a byte-for-byte clone of dest" is the same danger the guard
was built to catch, or a recognizable, safe special case of it.

No new prior art was needed here beyond what [decisions/0006](0006-setup-uses-real-shared-history.md)
already established: a clean clone of dest, prior to setup running, *is* dest's real
tip commit sitting in source's object database and reflected onto a local branch —
exactly the state 0006 already requires `setup` to produce for itself via its own
fetch. Recognizing it doesn't relax what 0006 requires; it recognizes that a user can
arrive at that same state by an ordinary `git clone` instead of only by `setup`
fetching it itself.

# Decision

Replace the "repo must be completely empty" precondition with a narrower one, checked
per branch against `config.branches` (so config is now parsed before this check runs,
reordering it ahead of where it sits in `setup.rs` today):

1. **Detached HEAD, or any local branch whose name isn't in `config.branches`, still
   hard-fails exactly as today** — unrecognized commits are unrelated history, not a
   clone of dest, and the existing error message stands.
2. **For each branch in `config.branches` that already exists locally**: after
   fetching dest's current tip for that name (already part of setup's existing
   fetch-everything-upfront pass), the local branch's tip must be identical (same
   oid) to that freshly-fetched dest tip. Identical → proceed exactly as if the
   branch didn't exist yet, grafting the config-file commit on top of what's already
   there. Different → hard-fail with a clear message ("source's local branch %s
   already has content that doesn't match dest's current tip — refusing to graft over
   independent history"), the same hard-stop policy [decisions/0007](0007-conflict-policy-hard-stop.md)
   already uses for every other real conflict this tool refuses to guess through.
3. **A branch in `config.branches` with no local branch yet** (e.g. a plain `git
   clone` only checks out dest's default branch locally; other branches exist only as
   remote-tracking refs, if they were fetched at all) is grafted exactly as setup does
   today for every branch in a genuinely empty repo — no behavior change for that
   case.
4. **Rollback must distinguish branches setup created from branches that already
   existed and were merely amended.** A branch that pre-existed (matched dest's tip
   before grafting) rolls back by resetting it to its original oid, not by deleting
   it — deleting a branch the user's `git clone` produced, on a later branch's
   unrelated failure, would be a worse outcome than what rollback is protecting
   against. Branches setup actually created from nothing still roll back by deletion,
   as today.

# Why

* **Matches the workflow a user reaches for before reading gitprism's own docs.**
  "Clone the repo I want a superset of" is standard practice; making `setup` reject
  it outright forces everyone through a less obvious sequence (empty dir, hand-write
  `.gitprism.toml`'s dest URL, let `setup` do a fetch you could have gotten via
  `clone`) for no safety gained once the clone-equals-dest-tip case is verified
  directly.
* **The safety property the original guard protects is preserved exactly, not
  loosened.** The only newly-accepted state is one where source's existing content is
  *proven* — by oid comparison against a fresh fetch, not assumed from clone having
  happened recently — to be dest's own unmodified tip. Any real independent history,
  including a previous `gitprism setup` run's own graft commits (which are always one
  commit ahead of dest's raw tip), still fails the oid check and hard-stops, same as
  today.
* **No new mechanism required.** setup already fetches every configured branch's dest
  tip upfront before committing anything (decisions/0006's rollback-safety pass); the
  oid comparison this decision adds reuses that exact fetch, just adds a lookup
  against `config.branches`'s already-existing local branch (if any) at the same
  point.
* **Consistent with 0007's hard-stop-over-guessing philosophy** rather than
  introducing a new "looks safe, proceed anyway" heuristic: the only thing being
  treated as safe is bit-for-bit identity with dest's own tip, not "recently cloned"
  or any other proxy for it.

# Consequences

* **Config must be parsed before the branch-existence check**, not after as
  `setup.rs` orders it today — a reordering, not a behavior change to parsing itself.
* **`setup.rs`'s precondition check changes from a single boolean gate to a per-branch
  oid comparison**, folded into the existing "fetch every branch's dest tip" loop
  rather than a separate pass.
* **`rollback_branches` needs to track, per branch, whether it pre-existed** (and at
  what oid) versus was created fresh this run, so a partial-run failure resets
  pre-existing branches instead of deleting them. This is the one real new piece of
  state setup needs to carry through the commit phase.
* **`graft_branch` itself is unchanged**: `repo.commit(Some(refname), ..., parents:
  &[&dest_tip])` already succeeds when `refname` currently points at exactly
  `dest_tip` — a fast-forward from the ref's current target — so amending a
  pre-existing, verified-matching branch needs no new commit logic, only the relaxed
  precondition around it.
* **Materializing the first configured branch's checkout is unaffected**: a clone
  already has that branch's working tree populated matching dest's content, and the
  existing "delete on-disk control files before checkout" step already handles the
  only new file content (`.gitprism.toml`/`.gitprismignore`) checkout needs to write.
* **A repo that already had `gitprism setup` run against it once still hard-fails**,
  since its branches sit one graft commit ahead of dest's raw tip — indistinguishable,
  under this decision's check, from any other real independent history. Re-running
  setup remains unsupported, unchanged from today.
* **Not decided here**: the exact wording of the new hard-fail message, or whether a
  local branch that exists in `config.branches` but was never actually fetched (e.g.
  a shallow or single-branch clone) should attempt its own fetch transparently before
  comparing — the existing fetch-every-branch pass already does this unconditionally,
  so no new fetch behavior is actually needed, but this is called out as a
  not-yet-written-test edge case for implementation time.
