---
type: Decision
title: Both sync directions merge via a real `git merge-tree` subprocess
description: Neither direction does its own merge or patch work any more. One primitive — `git merge-tree --write-tree` over pre-filtered trees — computes the resulting tree for source→dest and dest→source alike. Supersedes decisions/0014's diff-and-apply mechanism and amends decisions/0002's "local merge work goes through git2-rs" clause.
tags: [architecture, git-library, filtering, conflict-handling]
status: draft
generated: { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
---

# Context

[decisions/0014](0014-source-to-dest-becomes-diff-based-and-can-conflict.md) made
source→dest apply each pending commit as its own filtered diff, via `git2`'s
`diff_tree_to_tree` + `apply_to_tree`. An end-to-end review found three real bugs in
that mechanism (recorded in [log.md](../log.md)), and the two serious ones share one
root cause: **patch application is not idempotent.** Re-applying a change that is
already present doesn't no-op, it duplicates — so a merge commit in source put
`line1\nline1` on dest while reporting success, and an already-synced commit re-yielded
after an independent dest commit either duplicated content or failed to apply and was
misreported as a real conflict, bricking the pair.

Both were fixed, but the first fix's shape is what prompted this decision: it worked
around non-idempotency with a source-space cursor (diff from the previously examined
pending commit rather than from the commit's own parent). That restored correctness at
the tip while introducing a second mechanism for the same property, and it left a wart
— for an interleaved source history the intermediate dest commit for whichever branch
the walk emits second is a state that never existed on source, and a conflict
hard-stopping mid-chain can leave dest fast-forwarded *to* such a commit, verifiably
missing a file source had pushed one commit earlier.

The project owner's framing, which decided this: **what git already does well should be
done by git, not reinvented** — and, separately, **don't solve everything
automatically; let an operator do it.**

Prior art was checked before choosing (rule three: check how others solve this first):

* **josh** — the closest analogue. Its reverse-apply,
  [`unapply_filter` in `josh-core/src/history.rs`](https://github.com/josh-project/josh/blob/2ac70eab2710ef9302fe999e92b6b7cf961f4e2e/josh-core/src/history.rs#L378-L655),
  is not patch application: it calls `git2::Repository::merge_commits` twice, once
  with `FileFavor::Ours` and once with `Theirs`, and proceeds only if both resulting
  trees agree. When that check fails it hard-errors
  (`anyhow!("rejecting merge with N parents...")`) rather than guessing — the same
  operator-first policy as [decisions/0007](0007-conflict-policy-hard-stop.md). See
  [references/josh](../references/josh.md), whose open question about "the exact
  mechanics of the reverse merge" this answers.
* **jujutsu** — implements its own tree merge in pure Rust (`lib/src/tree_merge.rs`,
  `lib/src/merge.rs`) with an explicit same-change rule, `A+(A-B)=A`, documented as
  "what Git and Mercurial do (in the 3-way case at least)" — i.e. idempotency comes
  from merge algebra. Its conflict philosophy (materialize conflicts as first-class
  objects, never block) is the *opposite* of rule two and is deliberately not followed
  here. See [references/jujutsu](../references/jujutsu.md).
* **Copybara** writes a full transformed snapshot in a working tree via the real `git`
  binary and dies with git's own conflict text on a rebase conflict
  ([issue #104](https://github.com/google/copybara/issues/104)); resume state is a
  commit trailer, exactly as [decisions/0003](0003-mapping-state-in-commit-trailers.md)
  already does here. See [references/copybara](../references/copybara.md).
* **git-subtree** builds rewritten history with `git commit-tree` and uses real
  `git merge` for the merge direction; it requires a working tree
  (`require_work_tree`), which gitprism cannot. See
  [references/git-subtree](../references/git-subtree.md).
* **git itself** frames plain patch application as the fallback rather than the
  mechanism: `git am --3way` falls back *from* 3-way merge to patching, and
  `cherry-pick`/`rebase` are built on merge machinery, not patch text.
* **`git merge-tree --write-tree`** (git 2.38+) is git's own plumbing for exactly this
  shape — "intended as low-level plumbing, similar to git-hash-object(1),
  git-mktree(1)", touching neither the index nor the working tree. GitLab's Gitaly
  adopted it to perform server-side merges in memory
  ([Gitaly MR !4479](https://gitlab.com/gitlab-org/gitaly/-/merge_requests/4479)).
* **`git replay`** (2.44) was considered and rejected: it is explicitly experimental
  and does not handle merge commits or root commits, which is precisely the hard case
  here.

# Decision

Both directions delegate the merge to git.

* One primitive, used by both:
  `git merge-tree --write-tree --merge-base=<base> <ours> <theirs>`, a real
  subprocess. It accepts raw tree oids for all three arguments, so no throwaway
  commit objects are needed.
  * **source→dest**: base = the pending source commit's first-parent tree (the
    mainline parent for a merge commit), *filtered*; ours = dest's current chain-tip
    tree; theirs = the pending source commit's tree, *filtered*.
  * **dest→source**: base = the pending dest commit's first-parent tree (mainline for
    a merge commit); ours = source's current chain-tip tree; theirs = the pending dest
    commit's tree. Unfiltered — dest never holds source-only content.
* `apply_filtered_diff` (diff + `apply_to_tree`) and `cherrypick_commit` are both
  removed. Two engines becomes one, so the directions can never disagree about what
  counts as a conflict.
* **Filtering stays gitprism's own job**, as tree construction: build a filtered copy
  of a tree with excluded paths removed before handing it to git. That is not merge
  logic, and doing it *before* the merge preserves decisions/0014's requirement that an
  excluded path never reaches the merge at all — otherwise its own history looks like a
  modify/delete conflict on every sync, dest never having that path.
* **The source-space cursor is deleted** and the diff base returns to the commit's own
  first parent — what decisions/0014 originally specified. The cursor existed only to
  work around non-idempotent patch application; 3-way merge makes it unnecessary, and
  one mechanism per property is the point.
* **Conflict handling is unchanged in policy and better in detail**: exit status 0 is
  clean, 1 is a real conflict, anything else is an error. A conflict hard-stops the
  pair per decisions/0007 — gitprism never uses the conflicted tree merge-tree still
  writes — and `--write-tree -z --name-only` supplies the conflicted path names, so
  the hard-stop can now tell the operator *which files* to look at.
* Trailer bookkeeping ([decisions/0003](0003-mapping-state-in-commit-trailers.md)) is
  untouched and still does a different job: the merge gives content-level idempotency,
  the trailers prevent re-processing. josh, Copybara and git-subtree all keep separate
  bookkeeping for the same reason.
* [decisions/0015](0015-resolve-real-git-cherry-pick-explicit-continue.md) is
  unaffected: `gitprism resolve` still drives a real `git cherry-pick` against a real
  working tree, because a human needs ordinary conflict markers. `sync` staying in the
  object database and `resolve` using the working tree remains the deliberate split.

# Why

* Rule one, applied literally. Every close analogue abandoned patch application for a
  real 3-way merge, and git's own newer server-side primitives (`merge-tree`,
  `replay`) are merge-based. `merge-tree` is the only one of them that is
  non-experimental and handles merge commits.
* It fixes the root cause rather than its symptoms. Idempotency, correct merge-commit
  handling, no interleaved-branch churn, no stranding at synthetic states, native
  binary handling, and rename detection all follow from using a real merge instead of
  being separately engineered.
* Rule two is preserved and strengthened: no auto-resolution, hard-stop on a real
  conflict, and now a concrete file list for the human. josh's hard-error is the
  precedent; jj's never-block model is explicitly rejected.
* It makes the two directions symmetric. A cherry-pick *is* a 3-way merge, so keeping
  `cherrypick_commit` for one direction and something else for the other was an
  accident of implementation order, not a design.
* `merge-tree` needs no working tree, which `sync` requires (it runs in CI and builds
  commits straight into the object database) and which is exactly why git-subtree's and
  Copybara's checkout-based approaches don't transfer.

# Consequences

* **git >= 2.38 becomes a hard runtime requirement** (`--write-tree`). gitprism
  already requires the real `git` binary for push/fetch, so this narrows an existing
  dependency rather than adding one. Version should be detected and reported clearly
  rather than surfacing as a confusing parse failure.
* **decisions/0002's "local object-graph work — including the dest→source merge — goes
  through `git2-rs`" no longer holds for the merge.** `git2` remains in use for
  everything else local: revwalks, trailer scans, tree construction and filtering,
  commit building, ancestry checks. 0002's split survives; only its merge clause moves
  from the library to the subprocess side. 0002's stated reason for preferring git2 over
  gix — that gix's merge support was incomplete and "merge is core, not incidental" —
  is now moot for the same reason: the merge is git's.
* **decisions/0014's mechanism is superseded**, though its central finding stands: a
  full-tree snapshot replace silently regresses dest's independent content, so
  per-commit application against dest's current tip is required. Its "own filtered
  diff... via git2's diff + apply_to_tree" is replaced by "own filtered 3-way merge via
  git merge-tree", and its equivalence-for-linear-history argument becomes unnecessary
  rather than merely restated.
* One subprocess per pending commit, versus an in-process call. Irrelevant at CI
  scale, and cited benchmarks put `merge-tree` far below `git merge` for the same work.
* On the conflict path merge-tree writes a tree gitprism deliberately discards, leaving
  unreferenced loose objects until the repo is gc'd. Acceptable: ordinary git garbage,
  in a CI checkout, on an error path.
* Conflict *fidelity* changes slightly by design: ort's rename detection means some
  cases that previously failed to apply now merge correctly, and conflict boundaries
  match what a human running `git cherry-pick` would see — which is the point, since
  `resolve` reproduces the conflict with real git.
