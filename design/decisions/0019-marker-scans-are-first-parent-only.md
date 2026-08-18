---
type: Decision
title: Marker scans walk first-parent-only, not full ancestry
description: newest_source_marker and newest_dest_marker (design/log.md's "trailers are not pair-qualified" gap) now revwalk with git2's simplify_first_parent(), matching git rev-list --first-parent, instead of every reachable commit. A real, two-parent merge of a mirror-only branch into a round-tripped branch on dest can no longer make the round-tripped branch's own resume-point scan cross into the merged-in branch's own trailer history. Documented limitation: this assumes the tracked branch is first-parent of its own merges, true for GitHub/GitLab/Azure DevOps' "merge PR" button and for `git merge` run from the target branch.
tags: [architecture, state, mapping, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-17T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

[decisions/0003](0003-mapping-state-in-commit-trailers.md) established
commit-message trailers as the sole mapping/resume state, read back by two
scans in `src/commands/sync.rs`: `newest_source_marker(repo, dest_tip)` walks
dest's ancestry from `dest_tip` for the newest `Gitprism-Source-Commit`
trailer, and `newest_dest_marker(repo, source_tip)` walks source's ancestry
from `source_tip` for the newest `Gitprism-Dest-Commit` trailer. Both have
always walked *every* reachable commit (`git2::Sort::TOPOLOGICAL`, no further
restriction) — deliberately, per `newest_source_marker`'s own doc comment, so
a marker sitting behind an unexpected re-graft is never invisible to the
scan.

`design/log.md`'s "Open, not yet decided" section has flagged the resulting
gap since the code that introduced `newest_source_marker`/`newest_dest_marker`
first landed: "Trailers are not pair-qualified: both marker scans accept any
`Gitprism-*-Commit` trailer regardless of which branch pair wrote it, so dest
branches merged into one another can hand a pair the other pair's marker."

[decisions/0017](0017-source-to-dest-mirrors-every-branch.md) turned that
theoretical gap into a real one: source→dest now mirrors *every* branch on
source, unconditionally, so a mirror-only branch's own commits (each one
carrying its own `Gitprism-Source-Commit` trailer, decisions/0003) routinely
exist on dest. [decisions/0018](0018-branch-deletion-failure-modes.md)'s own
Case 2 test fixture had to work around exactly this while writing a squash-
merge-shaped fixture, and left an explicit note in its test body and in
`design/log.md` explaining why it deliberately avoided a real two-parent
merge: doing so "folded that commit's own `Gitprism-Source-Commit` trailer
into the round-tripped branch's ancestry," which "made `newest_source_marker`'s
unrelated, already-known ... gap misidentify the *feature* branch's own
commit as the round-tripped branch's resume boundary."

Reproduced directly for this decision: mirror a feature branch `B` to dest
(decisions/0017), then merge `B`'s dest tip into a round-tripped branch
`main`'s dest tip via a **real, two-parent** `git merge` (not a squash) — the
ordinary shape GitHub's/GitLab's "merge pull request" button and a manual
`git checkout main && git merge B` both produce, keeping `main` as the merge's
first parent and `B` as its second. `main`'s own dest→source sync, run in the
same `sync` invocation right afterward, needs `newest_source_marker` to find
`main`'s own boundary; instead the full-ancestry scan visits `B`'s own
`Gitprism-Source-Commit`-bearing commit (reachable via the merge's second
parent) before ever reaching a commit `main`'s own history actually wrote,
and returns `B`'s marker — a source-space oid on a branch `main` never
descends from. `dest_resume_point`'s subsequent
`graph_descendant_of(source_tip, boundary)` check correctly notices
`source_tip` isn't a descendant of that oid and refuses, hard-stopping the
whole sync with "isn't at a point this clone can safely build on" — even
though `main`'s own sync state is perfectly fine. A different topology (the
wrong marker happening to still be an ancestor) would instead pass that
check and resume from a stale boundary, silently re-processing already-synced
commits — the same class of bug, worse, because it fails silently instead of
loudly.

Prior art checked before answering: this is exactly the situation git's own
`--first-parent` history-simplification flag exists for — "follow only the
first parent commit upon seeing a merge commit," documented in `git-log(1)`
as the standard way to view "the history of [a] branch, disregarding what was
merged in from other branches" — and git2-rs exposes the identical primitive
as `Revwalk::simplify_first_parent()`, a thin wrapper over
`git_revwalk_simplify_first_parent()` (confirmed against the installed
`git2 = "0.21.0"` in `Cargo.lock`/`Cargo.toml`). No new mechanism, no new
state, no new transport — the fix is entirely a call already sitting next to
the two revwalks this codebase has used since decisions/0003.

# Decision

Both `newest_source_marker` and `newest_dest_marker` call
`revwalk.simplify_first_parent()` (alongside the existing
`revwalk.push(...)` and `revwalk.set_sorting(git2::Sort::TOPOLOGICAL)`, order
between the two calls doesn't matter to libgit2) before iterating. Once
set, a merge commit's non-first parents — and everything reachable only
through them — are never visited by that revwalk at all, so a merged-in
branch's own trailer-bearing commits can never be mistaken for the tracked
branch's own marker, because they're no longer reachable from the scan in
the first place.

Nothing else about either function changes: same signature, same trailer
key read, same "keep walking until a match, else `None`/`bail!`" shape,
same unbounded-walk behavior `newest_source_marker`'s doc comment already
argues for (unbounded now means "the whole first-parent line," not "the
whole reachable graph").

**Documented limitation, not engineered around**: this relies on the tracked
branch always being the first parent of its own merges. That is the default,
unremarkable case for a "merge pull request" button (GitHub, GitLab, Azure
DevOps all merge *into* the target branch, keeping it first) and for anyone
merging by checking out the target branch and running `git merge <other>` —
git always makes the currently-checked-out branch's tip the first parent.
It does not hold for a merge performed the other way around (checking out the
feature branch and merging main into it, then fast-forwarding main to that
result) or for history rewritten to swap parent order. gitprism does not
detect or guard against that inversion; it is accepted as a known limitation
of relying on git's own first-parent convention, the same convention
`git log --first-parent`, `git rev-list --first-parent`, and GitHub's own PR
merge commits already depend on.

# Why

* **git's own canonical answer to this exact class of problem.** `--first-
  parent` exists specifically so a branch's own linear story can be read
  without a merged-in branch's history intruding — precisely the property
  both marker scans need and didn't have.
* **No new state, ref, or transport.** Unlike pair-qualifying the trailer
  itself (e.g. stamping `Gitprism-Source-Commit[main]: ...`, which would
  need every existing trailer and every reader of one to change in lockstep,
  and would still need a decision about branches created before the change)
  or keeping a separate per-branch cursor (decisions/0003's "Consequences"
  already anticipated and declined this as a pure speed optimization, not a
  correctness requirement), this is a one-line change to how two existing
  revwalks are seeded.
* **Stays inside decisions/0003's architecture.** The trailer format, what
  it records, and how it's read back are all unchanged — only which commits
  the walk is allowed to reach changes.
* **Directly resolves the log's own long-standing "Trailers are not pair-
  qualified" entry** with a documented mechanism, rather than leaving it as
  an accepted-but-unaddressed gap indefinitely.
* **Matches the workflow decisions/0017 itself describes.** That decision's
  own "Why" section already assumes ordinary PR-merge topology ("finish the
  work, merge it into dest's main"); first-parent-only walking simply makes
  the marker scans agree with the same assumption the branch model already
  rests on.

# Consequences

* **The reproduced bug (real two-parent merge of a mirror-only branch into a
  round-tripped branch) no longer hard-stops or mis-resumes** — verified by
  a new regression test, `run_ignores_a_merged_in_branchs_own_trailer_when_resuming_after_a_real_merge`,
  that reproduces decisions/0018's own Case 2 fixture but with a *real*
  two-parent merge instead of the single-parent stand-in that test
  deliberately used, and asserts today's code fails before the fix and
  succeeds after it.
* **A branch merged the "wrong way round"** — feature branch first-parent,
  tracked branch second — is not detected or guarded against; its own
  marker scan can still cross into the tracked branch's history. Accepted as
  a documented limitation (see Decision), not solved here: nothing in the
  workflow decisions/0005/0017 describe merges that way, and detecting the
  inversion would need either a config-declared branch identity per commit
  or a much more expensive scan — not something the described workflow
  needs (rule: don't implement solutions for things we don't need).
* **`newest_dest_marker`'s "always finds something for a properly set-up
  branch" guarantee still holds**: `setup`'s own graft commit
  (decisions/0006) is a real, direct, first-parent ancestor of every branch
  it grafts, so the first-parent walk still reaches it.
* **`newest_source_marker`'s "deliberately unbounded, doesn't hide the graft
  point" property is unchanged in kind, narrower in scope**: the walk still
  never stops early looking for an *older* marker sitting behind the usual
  boundary, it just no longer looks down non-first-parent branches to find
  one. A marker that only exists off a merged-in side branch's own history
  was never the tracked branch's own resume point anyway.
* **No change to `pending_commits`, `dest_tip_is_accounted_for`'s cases 1/2,
  or `build_pending_dest_tip`/`build_pending_source_tip`'s own walks** —
  none of those revwalk full ancestry for marker discovery; the change is
  scoped to the two functions with the actual bug.
