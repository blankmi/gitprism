---
type: Decision
title: pending_commits walks first-parent-only, matching the apply step's own assumption
description: pending_commits (shared by both sync directions) gains revwalk.simplify_first_parent(), the same idiom decisions/0019 already applied to the marker scans. Closes the gap decisions/0019 explicitly left open — build_pending_dest_tip and build_pending_source_tip derive their three-way-merge base from a commit's first parent, but the walk feeding them emitted full-DAG history, so a merged-in branch's own commits were replayed against a base tree the dest/source chain was never actually at.
tags: [architecture, merge, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-21T00:00:00Z }
---

# Context

[decisions/0019](0019-marker-scans-are-first-parent-only.md) made the two
marker scans, `newest_source_marker` and `newest_dest_marker`, walk first-
parent-only, and said so explicitly in its own Consequences: "No change to
`pending_commits`, `dest_tip_is_accounted_for`'s cases 1/2, or
`build_pending_dest_tip`/`build_pending_source_tip`'s own walks — none of
those revwalk full ancestry for marker discovery; the change is scoped to
the two functions with the actual bug." That statement about
`pending_commits` is correct as far as it goes, but it leaves a second,
distinct bug in place that this decision now closes.

`pending_commits` (now `src/commands/sync/mod.rs`, around line 875 at the time) walks the
complete DAG between `boundary` and `tip`: `revwalk.push(tip)`,
`revwalk.hide(boundary)`, `Sort::TOPOLOGICAL | Sort::REVERSE`. For a merge
commit reachable in that range, it emits the merge's own side-branch
commits — not just the merge commit itself. `build_pending_dest_tip`
(around line 565) and `build_pending_source_tip` (around line 1812), the
two functions that actually apply each pending commit via
[decisions/0016](0016-both-directions-merge-via-real-git-merge-tree.md)'s
`git merge-tree`, each derive the three-way-merge base from
`commit.parent(0)` — pure first-parent semantics, unchanged since
[decisions/0014](0014-source-to-dest-becomes-diff-based-and-can-conflict.md).
The walk and the apply step disagree about what a merge commit means: the
walk hands the apply step a side branch's own commits, but the apply step's
base-tree choice only makes sense for commits on the tracked branch's own
first-parent line.

Reproduced directly for this decision: topology `R0 → R1 → R2` on the
landing branch, `R0 → F1` on a feature branch, and `M` a real two-parent
merge of `F1` into `R2` whose conflict a human resolved by hand in the
merge commit itself. The full-DAG walk emits `R1, R2, F1, M`. Applying
`F1` uses base = `R0`'s tree, ours = the `R2`-derived dest tip, theirs =
`F1`'s tree — `git merge-tree` exits 1 with a genuine content conflict,
hard-stopping the pair per [decisions/0007](0007-conflict-policy-hard-stop.md),
even though `M` already carries the human's resolution and would apply
cleanly. Under a first-parent walk the emitted set is `R1, R2, M`, and
applying `M` with base = `R2`'s tree exits 0 with the resolution intact.

Verified against real git that `--first-parent` restricts only the
*emitted* set, not what `hide()` can still reach: the hide boundary is
traversed through all parents regardless. With a boundary reachable only
via a merge's second parent, the root stayed hidden under the first-parent
walk exactly as it does today. The first-parent pending set is therefore
always a subset of today's full-DAG set — no commit `hide()` already
excludes can be re-emitted by restricting the walk further.

Applied the one-line change in a throwaway worktree and ran the full
suite: 190 passed, 0 failed, unchanged. The two existing merge tests,
`run_does_not_duplicate_a_no_ff_merges_content_on_dest` and
`run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once`,
assert only final dest-tip content, which is identical either way — the
suite cannot currently distinguish the two walks. Neither test covers a
merge whose conflict was resolved in the merge commit itself, which is the
shape that actually reproduces the bug.

`design/log.md` (the entry documenting decisions/0016) states that
decision made the interleaved-branch churn "disappear". It didn't, fully:
the test comment at (now) `src/commands/sync/tests/source_to_dest.rs`, in
`run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once`,
still records
that "whichever branch the revwalk ... emits second yields a dest commit
whose diff ... temporarily removes that other branch's file — restored
again by the merge commit's own diff", calling it "an accepted, recorded
design question for the project owner, not something this test asserts on
or tries to fix". That comment is accurate about today's code: 0016
replaced *how* each pending commit is applied (patch → three-way merge)
but did not change *which* commits `pending_commits` emits, so a side
branch's commits are still individually replayed onto the landing
branch's dest history before the merge commit's own diff restores the
other branch's content. The log entry's "disappear" claim describes the
duplication bug 0016 fixed, not this churn; this decision is what actually
removes the churn, by removing the side-branch commits from the emitted
set in the first place.

# Decision

`pending_commits` calls `revwalk.simplify_first_parent()`, mirroring the
idiom already used at `src/commands/sync.rs` (at the time; now split across `sync/marker_scan.rs` and `sync/anchor.rs`) lines ~1583 and ~1694
(decisions/0019). Because both sync directions share `pending_commits`,
one change covers source→dest and dest→source alike. A merge commit is
now carried through the apply step as a single net change against its
first parent, and a conflict resolution recorded by a human in the merge
commit is honored rather than re-litigated against the side branch's own
diff.

Same assumption decisions/0019 already documented: this relies on the
tracked branch being the first parent of its own merges — the default for
GitHub/GitLab/Azure DevOps' "merge PR" button and for `git merge` run from
the target branch. If a merge was made the other way round (feature branch
checked out, target merged into it, then fast-forwarded), the side
branch's commits are on the first-parent line instead and are emitted as
before; the tracked branch's real commits are the ones now at risk of
being skipped from that merge's perspective. Not detected or guarded
against, for the same reason decisions/0019 gives: no config-declared
branch identity exists to tell the two apart, and the described workflow
doesn't produce merges that way.

# Why

* Makes the walk and the apply step agree for the first time. Under
  first-parent simplification, consecutive emitted commits are
  `parent(0)`-linked, and the first emitted commit's `parent(0)` is an
  ancestor of `boundary` — already synced. That is exactly the precondition
  `build_pending_dest_tip`/`build_pending_source_tip`'s `parent(0)` base
  already assumes; nothing in either function changes.
* Safe with respect to `hide()`: verified the first-parent set is a subset
  of the full-DAG set, so no already-synced commit can be newly emitted.
* One line, reusing a primitive already in this codebase for the identical
  reason (decisions/0019), rather than a new mechanism.
* Matches the domain on both sides. dest→source's requirement
  (`requirements/0001`, "changes made on dest ... sync back into source's
  corresponding branch") is import-what-was-actually-merged, which is what
  first-parent history records. source→dest's objection — that a feature
  branch's own commits become invisible to the landing branch's dest
  history — is answered by [decisions/0017](0017-source-to-dest-mirrors-every-branch.md):
  every source branch is mirrored to dest under its own name regardless, so
  those commits still reach dest, just on their own branch rather than
  replayed a second time inside the landing branch's history.
* Removes the interleaved-branch churn
  `run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once`'s
  own comment still records as unresolved: a merge is now carried as one
  net change against its first parent instead of as a side branch's
  commits followed by a merge restoring what they temporarily removed.

# Consequences

* **Conflict resolutions recorded in a merge commit are honored.** The
  reproduced bug above no longer hard-stops; applying the merge commit
  against its first-parent base carries the human's resolution instead of
  re-deriving a conflict from the side branch's own diff.
* **The interleaved-branch churn is resolved**, correcting `design/log.md`'s
  "disappear" claim about decisions/0016: that decision fixed duplication
  by changing *how* a commit is applied, not *which* commits are emitted,
  so the churn described in
  `run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once`'s
  comment survived it. This decision removes the side-branch commits from
  the emitted set, which is what actually removes the churn.
* **Side-branch commits no longer appear inside a landing branch's dest
  history.** They still reach dest, on their own mirrored branch, per
  decisions/0017. A dest clone that only ever looks at the landing branch
  sees the merge as one commit instead of the side branch's history
  followed by it.
* **`MAX_PENDING_COMMITS` now bounds a smaller set** for any range
  containing merges — side-branch commits no longer count against the
  10,000-commit limit (`src/limits.rs`), only first-parent-line commits do.
* **First-parent assumption and its failure mode**, same as decisions/0019:
  if the tracked branch is not first-parent of its own merges, its own
  commits can be the ones simplified away from that merge's perspective.
  Not detected; accepted for the same reason 0019 accepted it.
* **Migration**: a dest repo already synced under the old, full-DAG
  `pending_commits` may already contain a prior merge's side-branch commits
  in its history, applied individually before this decision. The first
  merge processed under first-parent semantics after upgrading uses a base
  tree (the merge's first-parent tree) that dest has already partly moved
  past — dest's actual tip already has some of that content via the old
  side-branch replay. `git merge-tree` still computes a correct three-way
  merge against dest's real current tip regardless of how that tip was
  built, so this is not expected to misbehave, but it is unverified and
  needs its own test: a dest history built by the old walk, followed by a
  new merge processed by the new walk.
* **Unverified by the existing suite at this commit.** No code changed
  here; `cargo test` still passes 190/190 either way, which is exactly the
  problem — the suite cannot distinguish the two walks. The implementation
  commit must add:
  * a merge whose conflict was resolved by hand in the merge commit itself
    (the regression that proves the bug, reproduced in Context above);
  * a clean two-parent merge with no conflict;
  * a boundary reachable only via a merge's second parent, confirming
    `hide()` still keeps it hidden under `simplify_first_parent()`;
  * squash-merge and rebase/linear PR completion, as regression cases —
    both are single-parent, so unaffected by this change; they confirm
    existing behavior, not the fix;
  * the dest→source direction with a real two-parent merge on dest, since
    `pending_commits` is shared and dest→source has no equivalent coverage
    today;
  * the migration case above: dest history built under the old full-DAG
    walk, then a new merge processed under the new first-parent walk.
