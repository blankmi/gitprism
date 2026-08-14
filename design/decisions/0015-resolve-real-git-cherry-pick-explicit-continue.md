---
type: Decision
title: gitprism resolve drives a real git cherry-pick and resumes via an explicit --continue flag
description: Fills decisions/0008's deferred gap. `gitprism resolve <pair>` shells out to a real `git cherry-pick` (not git2) so a conflict leaves ordinary working-tree markers, identifies the commit to resolve the same way sync would build it next, and resumes only on `gitprism resolve <pair> --continue`, mirroring git's own rebase/cherry-pick/merge convention.
tags: [architecture, safety, conflict-handling, ux]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
---

# Context

[decisions/0008](0008-ship-resolve-helper.md) committed to shipping `gitprism resolve`
but explicitly deferred two things: how it identifies which pair/commit, and (not
named outright, but implied by "gitprism wraps `cherry-pick --continue`") how the
human tells gitprism the resolution is done. Both apply symmetrically now that
[decisions/0014](0014-source-to-dest-becomes-diff-based-and-can-conflict.md) means
either sync direction can hit a real conflict, not just dest→source.

Two real constraints shape the answer:

* `sync` itself never touches the working tree when it cherry-picks/applies a diff —
  it builds commits straight in the object database (`git2`'s `cherrypick_commit`/
  `apply_to_tree`) so a clean pending chain never needs a checkout at all. That's
  exactly wrong for `resolve`: decisions/0008 promises the human ordinary git conflict
  markers to edit, which only exist if the conflict is reproduced against the actual
  working tree and index, via a real `git cherry-pick` subprocess.
* Plain `git cherry-pick` auto-commits using whatever `user.name`/`user.email` this
  checkout's own git config has, not gitprism's configured committer identity, and
  doesn't append a `Gitprism-Dest-Commit` trailer. Decisions/0003 and 0010 aren't
  optional just because a human's hands touched this commit instead of gitprism's
  own diff/cherry-pick machinery.

# Decision

**Identifying the commit**: `gitprism resolve <pair>` (pair identified by
`source_branch`, matching the identifier `sync`'s own error output already prints)
recomputes dest→source's pending list exactly the way `sync` would — same boundary
lookup (`newest_dest_marker`), same ancestor check, same
`Gitprism-Source-Commit`-trailer loop-prevention filter — and acts on the *oldest*
still-pending dest commit. No new state to track: since `sync` always pushes
everything that applies cleanly before stopping (decisions/0007), the oldest pending
commit *is* the one that conflicted, or the one to attempt next if nothing has
actually conflicted yet.

**Reproducing it**: runs a real `git cherry-pick <dest-sha>` subprocess (not git2)
against the currently checked-out branch (bails if `pair`'s source branch isn't the
one checked out). A clean apply and a real conflict are told apart by cherry-pick's
own exit code (`0` clean, `1` conflict needing resolution — git's own convention,
distinct from `128` for an unrelated fatal error, which surfaces as a plain `Err`).
`GIT_EDITOR=true` suppresses the commit-message prompt either way, since the
resulting commit's author/committer/message all get replaced in the next step
regardless of whether a human's hands were involved.

**Finishing it**: whether the apply was clean immediately or clean only after the
human resolved it, gitprism never keeps git's own auto-committed result. It reads the
tree that commit produced, then rebuilds it as gitprism's own commit object —
original author preserved, gitprism's configured identity as committer, `sync`'s own
`Gitprism-Dest-Commit` trailer appended (decisions/0003, 0010) — via the exact same
`build_source_commit` helper `sync` uses, so the two paths can never produce
differently-shaped commits. The local branch ref is then moved from git's auto-commit
to gitprism's replacement (a plain, non-forced local update — the auto-commit being
replaced is this operation's own just-created artifact, not independent work), and
the result is pushed to source, ff-only, the same as `sync` (decisions/0009).

**Resuming**: `gitprism resolve <pair> --continue` — an explicit flag, not the same
bare command re-run detecting `.git/CHERRY_PICK_HEAD` on its own. Matches git's own
established convention for every other resumable operation
(`rebase`/`cherry-pick`/`merge --continue`) instead of inventing a new implicit
detection rule. Bails clearly if no cherry-pick is in progress for `pair`, or if the
index still has unresolved conflicts.

# Why

* A real subprocess `git cherry-pick` is the only way to give the human decisions/0008's
  promised "100% standard git" experience — conflict markers in real files, `git add`,
  nothing gitprism-specific to learn. `sync`'s own git2-only cherry-pick was never a
  candidate here; it was built to avoid ever touching the working tree, the opposite
  of what a human resolving a conflict needs.
* Rebuilding the commit afterward, rather than trusting git's auto-committed result or
  amending it in place, means `resolve`'s output is provably identical in shape to
  `sync`'s (reusing `build_source_commit` directly) — the trailer/committer invariant
  decisions/0003 and 0010 depend on for every other commit gitprism creates isn't
  weakened just because this one path involved a human.
* An explicit `--continue` costs nothing new to learn on top of ordinary git and
  avoids a subtle failure mode implicit detection would have: re-running the bare
  command while a resolution is genuinely still in progress would otherwise need to
  silently guess "start over" vs "finish this" from filesystem state alone.
* Identifying the commit by recomputing the same pending list `sync` would build,
  rather than accepting a raw sha on the command line, means there's no separate
  identifier for a human to get wrong or copy stale — `resolve` and `sync` can never
  disagree about which commit is next.

# Consequences

* `resolve` only ever acts on the single oldest pending commit, matching decisions/0007's
  "order must be preserved" — it does not offer to skip ahead or batch-resolve several
  conflicts in one invocation. Resolving one, then running `gitprism sync` (or
  `resolve` again) is how a run of several conflicts in a row gets worked through.
* If the push at the end of `resolve` loses a fast-forward race or otherwise fails,
  gitprism does not retry it the way `sync` retries a lost race (decisions/0009) — the
  local branch is left correctly stamped (safe: it's a local-only ref move of this
  operation's own commit, not a force over independent work) and the failure message
  says to push it manually. `resolve` is a rare, human-supervised path; an automatic
  refetch-and-rebuild loop layered on top of an in-progress, possibly-conflicted local
  cherry-pick was judged not worth the added complexity for how infrequently this
  runs. Can be revisited if it turns out to matter in practice.
* This decision is written for dest→source's conflict shape specifically (cherry-pick).
  Source→dest's own conflict (decisions/0014, a failed diff apply rather than a
  cherry-pick) is not yet wired into `resolve` — tracked as a follow-up, not solved
  here.
</content>
