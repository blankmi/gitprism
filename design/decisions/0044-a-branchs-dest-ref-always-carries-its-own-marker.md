---
type: Decision
title: A branch's dest ref is never created or rebuilt at a commit its own dest_tip_accounted_for check wouldn't recognize
description: When build_pending_dest_tip built nothing for a branch whose dest ref is being created or rebuilt (a brand-new branch, or a detected mirror-only rewrite with nothing new to build), the fallback tip is no longer used verbatim — dest_tip_is_accounted_for is checked for that exact branch name first, and if it wouldn't recognize the anchor on a future run, a content-empty, branch-scoped marker commit is built on top of it instead. Fixes decisions/0043's sibling-anchoring extending a branch's dest ref onto another branch's own marker commit, which made every later resync of that branch bail permanently.
tags: [architecture, branches, markers]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-27T00:00:00Z }
---

# Context

A repository review (finding F-02) reproduced a permanent-refusal bug
introduced by [decisions/0043](0043-mirror-only-branches-graft-onto-their-nearest-mirrored-ancestor.md):

Under 0043, a discovered branch's dest anchor may be a *sibling* branch's
own dest tip — e.g. `task`, forked from mirror-only `feature` with no
commits of its own yet, anchors on `feature`'s own dest commit rather than
falling back to the coarser graft. `sync_pair_to_dest_with_key`
(`src/commands/sync.rs`) then computed the new dest tip as:

```rust
let new_dest_tip = build
    .new_tip
    .or((force_rebuild || !dest_ref_exists).then_some(dest_tip));
```

When `build.new_tip` is `None` — `task` has nothing beyond the fork point,
so `pending_commits` is empty and `build_pending_dest_tip` never enters its
loop — this falls back to `dest_tip` itself: `feature`'s own dest commit,
used verbatim as `task`'s new ref.

That commit carries a gitprism marker for `feature`, not for `task`
([decisions/0003](0003-mapping-state-in-commit-trailers.md): markers are
branch-scoped, `marker::verify` checks the branch name). On `task`'s very
next sync, `dest_tip_accounted_for` (`src/commands/sync.rs`) checks three
cases for `task`'s dest tip, all against the same commit:

1. Case 1 (`marker::verify(..., "task", ...)`) fails — the marker names
   `feature`.
2. Case 2 (`dest_tip == graft_point`) fails — `feature`'s dest tip is past
   the graft.
3. Case 3 (`newest_dest_marker` scanning source's history for a
   `Gitprism-Dest-Commit` naming this exact dest tip, for branch `task`)
   fails — no such marker was ever written for `task`.

`dest_tip_is_accounted_for` returns `false`, `dest_resume_point_for_branch`
refuses, `mirror_only_rewrite_detected` also returns `false` (its own first
check requires `ViaPriorGitprismSync`, which just failed), and the call
falls through to the unconditional refusal — reachable as a per-branch
halt after [decisions/0045](0045-discovered-branch-refusals-are-per-branch-halts.md),
fatal before it. Either way, `task` never syncs again — not on the next
run, not once `task` gains a real commit of its own — until an operator
deletes its dest ref by hand.

The same shape arises from 0038/0039's rewrite-rebuild arm (`force_rebuild`)
whenever the graft-derived rebuild base isn't itself accounted for under
`task`'s own name, and from a round-tripped branch reaching step 5 of
0043's search and landing on an equal-`cbase` sibling. All three share one
root cause: the fallback path used `dest_tip` as the new ref target without
checking whether *this branch's own* `dest_tip_is_accounted_for` check
would recognize it — the very check the ref's own next sync depends on.

# Decision

Before using `dest_tip` (the anchor: a sibling's dest tip per 0043, or a
rewrite's graft-derived rebuild base) as the branch's new ref target,
`dest_tip_is_accounted_for` is checked for *this exact branch name*. If it
already would recognize `dest_tip` (the ordinary case: a brand-new branch
anchored directly at the shared graft, decisions/0006's Case 2, or a
rebuild landing back on a commit that already carries this branch's own
marker), `dest_tip` is used unchanged — no new commit, matching today's
behavior exactly.

If it would not, a single content-empty, branch-scoped marker commit is
built on top of `dest_tip` — same tree, `dest_tip` as sole parent, message
built by the existing [`build_dest_commit`](0016-both-directions-merge-via-real-git-merge-tree.md)
(the same function every real filtered commit already goes through),
carrying this branch's own `Gitprism-Source-Commit` trailer naming
`source_tip`. That commit becomes the new ref target instead.

This is the same asymmetry decisions/0003 already licenses on the other
side: `build_pending_source_tip` (dest→source) writes a marker commit even
when a cherry-pick merges to no tree change, specifically because "the
resume boundary *is* the newest `Gitprism-Dest-Commit` trailer... skipping
it would leave that trailer pointing at an older dest oid forever" — the
identical reasoning, applied to source→dest's own resume boundary
(`Gitprism-Source-Commit`) instead.

The check and the (possible) commit build are both scoped to exactly the
existing condition that already gated using `dest_tip` at all —
`build.new_tip.is_none() && (force_rebuild || !dest_ref_exists)`. An
ordinary no-op resync (`build.new_tip` is `None`, ref already exists, not a
rewrite) is untouched: nothing is pushed, matching today's behavior — this
decision never creates a marker commit on every unchanged run, only the one
time a branch's ref is first created or rebuilt at an anchor that isn't
already its own.

`dest_tip_is_accounted_for` is reused as-is, not duplicated — it already
answers exactly the question this decision needs ("would a future run
recognize this dest tip for this branch"), and it is the same check
`dest_resume_point_for_branch` and `mirror_only_rewrite_detected` already
depend on, so there is exactly one definition of "accounted for" in the
codebase.

# Why

* **The bug is a missing instance of an existing, already-decided rule.**
  Decisions/0003 already establishes that a resume boundary must always be
  backed by a real marker on the resuming side; this decision doesn't add a
  new rule, it closes the one place source→dest's own fallback path forgot
  to apply it.
* **Bounded to the exact condition that already used `dest_tip`
  verbatim.** No new branch class is affected; a genuinely brand-new branch
  anchored at the graft, and a rewrite rebuilt back onto its own prior
  marker, both take the unchanged fast path (no new commit) exactly as
  before.
* **Reusing `dest_tip_is_accounted_for` rather than duplicating its
  logic**, per this project's working style — one definition of "safe to
  build on," consulted everywhere that question is asked.
* **Reusing `build_dest_commit` rather than a second commit-building
  path** — the branch-scoped marker commit is built through the exact same
  function every other dest commit goes through, so it carries the same
  author/committer/marker shape as every other dest-bound commit gitprism
  ever writes; there is no second, parallel "marker-only" commit format to
  keep in sync with the real one.

# Consequences

* A brand-new branch with no commits of its own, anchored on a sibling
  (0043) or the graft (unchanged), now gets its own dest ref that
  `dest_tip_accounted_for` recognizes on every future run — no more
  permanent bail once the anchor isn't this branch's own graft point.
* One additional, content-empty commit appears on such a branch's dest
  history the first time it's created/rebuilt at a non-self-accounted
  anchor. Its tree is byte-identical to its parent's; nothing observable on
  dest's working tree changes.
* The reporter's completion line distinguishes "rebuilt to the anchor
  unchanged" from "rebuilt with a new branch-scoped marker commit" for the
  rewrite-rebuild arm specifically (the arm decisions/0039's own addendum
  already had wording for) — never misreporting a marker-only commit as "no
  new commits were needed."
* **Tests the implementation commit adds:**
  * `task` forked from mirror-only `feature` with no commits of its own:
    first sync creates `task`'s dest ref one commit ahead of `feature`'s
    own, carrying no content change and `task`'s own marker; a second,
    unchanged resync is a genuine no-op (no new commit, no bail); a third
    run, after `task` gains a real commit, syncs that commit normally on
    top of its own marker.
  * The existing brand-new-branch-at-the-graft test
    (`run_mirrors_an_ad_hoc_branch_with_no_commits_of_its_own`) is
    unaffected — confirmed by running it: the graft is already accounted
    for via Case 2, so the fast path (no new commit) still applies.

# Prior art

Not independently researched; this decision closes a gap in
[decisions/0043](0043-mirror-only-branches-graft-onto-their-nearest-mirrored-ancestor.md)'s
own mechanism and reuses [decisions/0003](0003-mapping-state-in-commit-trailers.md)'s
already-cited reasoning and prior art wholesale — see that decision and its
own addenda for the underlying marker-commit precedent.
