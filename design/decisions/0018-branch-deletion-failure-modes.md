---
type: Decision
title: A round-tripped branch's missing dest ref fails loudly; a mirror-only branch's missing dest ref is checked against merge status before recreating
description: Two branch-deletion failure modes decisions/0017 explicitly deferred, now resolved differently on purpose. A round-tripped branch (config.branches) with no ref on dest is a genuine error — source and dest have gone out of sync — and sync now fails with a clear, gitprism-authored message instead of leaking git's raw fetch error. A mirror-only (discovered) branch with no ref on dest is checked, content-first and with no persisted state, against whether it's already fully merged into a round-tripped branch's current tip before being rebuilt and pushed — if so, its absence on dest is treated as expected post-merge cleanup, not something to resurrect.
tags: [architecture, branches, error-handling]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-17T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

[decisions/0017](0017-source-to-dest-mirrors-every-branch.md) settled that gitprism
never deletes a branch on either side, and explicitly deferred "what happens to a
branch deleted on [dest]... any other deletion-adjacent edge case" as "not something
the described workflow currently needs." Manual testing against `src/commands/sync.rs`
surfaced two concrete cases of exactly that, with opposite correct answers:

**Case 1 — a round-tripped branch's dest ref disappears.** `sync_pair_from_dest`
unconditionally calls `git::fetch(source_root, &dest_url, branch)` for every branch in
`config.branches`, with no existence check first. If dest's copy of a round-tripped
branch (e.g. `develop`) is deleted — accidentally, or by some process gitprism doesn't
know about — that fetch fails with git's own raw subprocess error
(`fatal: couldn't find remote ref develop`), which aborts the whole run with no
indication of what actually went wrong or what to do about it. Unlike a mirror-only
branch's *first* sync (decisions/0017's "no dest ref yet" case, already handled by
`sync_pair_to_dest` via `git::remote_ref_exists`), a round-tripped branch is always
grafted by `gitprism setup` (decisions/0006) — its dest ref has no legitimate reason
to be missing. This is a real error condition, not a silent no-op: source and dest are
now out of sync in a way gitprism can't safely proceed past.

**Case 2 — a mirror-only branch's dest ref disappears after being merged.**
`sync_pair_to_dest`'s `!dest_ref_exists` branch cannot tell "this mirror-only branch
has never been synced to dest" apart from "this branch WAS synced, then merged into a
round-tripped branch on dest via an ordinary PR, and dest deleted the now-merged
source branch as routine cleanup" — both look identical (no ref, some ancestry back to
a graft or marker). Today's code treats both the same way: rebuild the chain from
source's own history and push it, unconditionally recreating a branch dest had every
reason to clean up. This is the opposite of Case 1: not an error, but gitprism
actively undoing an operator's/PR-tool's legitimate action.

Prior art was checked before choosing how to tell these two cases apart (rule three):

* **GitLab's own push-mirror feature** — the closest analogue to what
  decisions/0017 already asks for (mirror by name, no config entry) — documents
  exactly this asymmetry as its designed behavior: "When a branch is merged into the
  default branch and deleted in the source project, it is deleted from the remote
  mirror on the next [push]. Branches with unmerged changes are kept."
  ([GitLab docs, Push mirroring](https://docs.gitlab.com/user/project/repository/mirror/push/)).
  GitLab computes this live, per push, from the source project's own current state —
  no separate deletion ledger is described or implied.
* **git-trim** ([foriequal0/git-trim](https://github.com/foriequal0/git-trim)) is a
  real, shipped tool built entirely around this classification problem for local
  branches — "merged" vs "stray" — with no persisted state at all: every run
  re-derives the answer from the repository's current object graph. Its README is
  explicit that this is content-based, not oid-ancestry-based: it "can detect common
  merge styles such as merge with a merge commit, rebase/ff merge and squash merge,"
  the last of which has no ordinary ancestor relationship between the branch tip and
  the base branch at all — only a tree-content comparison can recognize it. This
  directly confirms the shape gitprism needs: `git branch --merged`-style ancestry
  checks are insufficient once squash merges are in scope, but a real 3-way merge (or
  equivalent tree comparison) still works.
* **decisions/0016** already established the primitive gitprism uses for every other
  content question in this codebase: a real `git merge-tree --write-tree` 3-way merge,
  not a hand-rolled diff or oid-ancestry check. Reusing it here rather than adding a
  second merge engine is the same "one mechanism per property" reasoning decisions/0016
  itself gives for removing `apply_filtered_diff`/`cherrypick_commit` in favor of one
  shared primitive.

# Decision

**Case 1 — round-tripped branch, missing dest ref: hard fail, don't guess.**
`sync_pair_from_dest` now checks `git::remote_ref_exists(source_root, &dest_url,
branch)` before fetching — the same existence-check idiom `sync_pair_to_dest` already
uses for a mirror-only branch's first sync, but with the opposite conclusion once it
comes back `false`: this is `config.branches`, so the ref not existing is never
routine. `anyhow::bail!` with a message naming the branch, stating plainly that source
and dest are out of sync, and telling the operator to investigate — not git's raw
"couldn't find remote ref" text. This is decisions/0007's hard-stop-don't-guess
policy applied to a new failure shape, not a new policy.

**Case 2 — mirror-only branch, missing dest ref: check merge status before
recreating, no persisted state.** Before `sync_pair_to_dest` rebuilds and pushes a
mirror-only branch (one absent from `config.branches`) that has no ref on dest yet, it
now asks: for each branch `M` named in `config.branches`, using `M`'s *current* local
tip on source (already up to date — `run()` finishes dest→source for every configured
branch before source→dest ever discovers a branch, decisions/0017's phase ordering) —

1. Compute `merge_base(branch_tip, M_tip)` in source's own history.
2. If there's no merge-base (unrelated history), or `branch_tip` equals the merge-base
   itself (the branch has no commits of its own beyond where it diverged from `M` —
   decisions/0017's own "freshly branched off an already-synced tip with no commits of
   its own yet" case, not a merge-and-cleanup situation at all — there's nothing to
   have been merged or cleaned up), skip this `M` and try the next.
3. Otherwise, run the same `git merge-tree` primitive decisions/0016 already uses:
   base = the merge-base's tree, ours = `M`'s current tree, theirs = `branch`'s tip
   tree — content-based, so a squash merge (no oid ancestry to `M` at all) is
   recognized exactly like a real merge or a rebase would be, matching git-trim's own
   reasoning for why oid ancestry alone isn't enough.
4. If the merge is clean and the resulting tree equals `M`'s own tree unchanged (the
   same no-op idiom `build_pending_dest_tip` already uses to decide not to push an
   empty commit), `branch`'s content is already fully present in `M` — treat the
   missing dest ref as expected post-merge cleanup (GitLab's own behavior for its push
   mirror), log an explanatory message, and do **not** recreate the branch on dest.
5. If no configured landing branch satisfies this (including when `config.branches`
   is empty — nothing to check against), fall through to the existing behavior:
   rebuild the chain and push it, unchanged from today.

No new state is written or read anywhere for this — not a trailer, not a file, not a
flag. The check is entirely a function of the current object graph, exactly like
git-trim's own classification.

# Why

* The two cases get opposite treatment because they *are* opposite: Case 1 is data
  going missing that should never go missing (round-tripped branches are gitprism's
  own explicit contract with the operator, established at `setup` time); Case 2 is
  data going missing for a reason entirely outside gitprism's control (a PR getting
  merged and its source branch cleaned up), which is the normal, expected lifecycle
  decisions/0017 already described for feature branches.
* Content-based, not oid-ancestry-based, because the workflow decisions/0017 itself
  documents — "finish the work, merge it into dest's main... have that merge synced
  back to source" — goes through dest→source's own cherry-pick/merge-tree mechanism
  (decisions/0016), which builds a *new* commit object on source, not a fast-forward of
  the original branch tip. A plain "is `branch_tip` an ancestor of `M_tip`" check would
  never recognize this as merged; git-trim's own README calls out the identical gap for
  squash merges specifically, and this codebase's dest→source path is architecturally
  closer to a squash (one new commit capturing the whole change) than to a fast-forward.
* Reuses `git::merge_tree` rather than inventing a second comparison mechanism —
  decisions/0016's whole point ("two engines becomes one, so the directions can never
  disagree about what counts as a conflict") extends naturally to "one content-equality
  primitive, not two."
* No persisted state, matching git-trim's own design and decisions/0017's existing
  "gitprism never deletes a branch" stance: the check only ever *withholds* a create it
  would otherwise perform, on evidence available in the object graph *this run*. It
  never touches dest, never remembers a past decision from run to run, and a branch
  that later gets new, unmerged commits pushed to it on source is recreated normally
  the very next run (the check is re-evaluated from scratch every time, exactly as
  decisions/0004's "current state governs, not history" principle already establishes
  elsewhere in this codebase).
* GitLab's push mirror is the direct precedent for treating this as *expected*
  behavior worth a log line, not an error: it is documented, intentional product
  behavior for the closest thing to gitprism's own source→dest direction that exists
  in real, shipped software.

# Consequences

* **Case 1 turns a whole class of "confusing raw git error" failures into one clear,
  actionable message** — the same improvement decisions/0017 already made for a
  mirror-only branch's *first* sync (`git::remote_ref_exists` before fetching), applied
  to the case that check deliberately didn't cover.
* **Case 2 means a mirror-only branch can permanently stop being mirrored** the moment
  its content lands in a round-tripped branch and its dest ref is removed — by design,
  since gitprism itself never deletes branches (decisions/0017) and this is the one
  case that would otherwise silently work against an operator's/PR-tool's own
  branch-hygiene action every single run.
* **A branch that looks "already merged" purely by coincidence** (not actually the
  product of a real PR merge, e.g. two unrelated branches that happen to converge on
  identical content) is treated identically — the check has no way to distinguish
  intent from content, which is the same limitation git-trim itself has and accepts.
* **One extra `merge_base` + `merge_tree` call per mirror-only branch with no dest ref,
  per configured landing branch** — bounded by `config.branches`' size, which
  decisions/0017 already established stays small (typically one or two long-lived
  branches), so this is not a scaling concern.
* **The guard in step 2 (branch tip equals its own merge-base with `M`) is required to
  avoid a false positive**, not merely an optimization: without it, a brand-new branch
  with zero commits of its own is trivially "already merged" into any landing branch it
  was cut from (their trees are identical), which would wrongly suppress
  decisions/0017's own "mirror even a branch with no commits of its own yet" guarantee.
  This was caught by the existing regression test for that case, not foreseen by this
  decision's first draft.
* **Still no deletion anywhere** — this decision only changes whether gitprism
  *recreates* something dest already let go of; decisions/0017's "gitprism never
  deletes a branch on either side" is untouched.

# Addendum (2026-08-18): the check must compare filtered trees, not raw ones

**What was wrong.** `already_merged_into_a_landing_branch`'s three-way merge compared
`base_tree`/`landing_tree`/`branch_tree` straight off each commit's raw, unfiltered
source-side tree. Every other cross-side content comparison in `sync.rs` —
`build_pending_dest_tip` most directly, the function this one exists alongside —
filters each tree through `filter_tree`/the current exclude-list first, because dest
only ever sees the filtered subset of source (decisions/0004, 0011). This function
never did.

**Why it mattered.** A mirror-only branch's own commits touching an excluded path
(anything listed in `.gitprismignore`) alongside their ordinary mirrored changes is
completely routine in a source-is-a-superset repo — it's the entire reason an
exclude-list exists. When that happened, the raw `theirs` tree carried content (the
excluded path) the landing branch's raw `ours` tree never had and never will, since
dest never received it either way. The merge still came back clean, but
`merged != landing_tree`, so the function concluded "not merged" — even though
everything dest would ever actually see from this branch had already reached the
landing branch. `sync_pair_to_dest` then fell through to its ordinary rebuild-and-push
path, resurrecting the branch on dest and undoing the PR's own cleanup, every single
sync run, forever. Not an edge case: the ordinary case for any project that excludes
anything at all.

**The fix.** `already_merged_into_a_landing_branch` now takes the same `exclude_list`
`sync_pair_to_dest` already loads once per run via `load_current_exclude_list` — passed
in as a parameter from the caller, not reloaded redundantly — and filters `base_tree`,
`landing_tree`, and `branch_tree` through `filter_tree` before handing them to
`git::merge_tree`, the identical primitive `build_pending_dest_tip` already uses for
its own base/theirs trees. No new state, no second filtering mechanism: this makes the
question the function asks correctly "is everything dest would ever see from this
branch already in the landing branch," rather than "is 100% of this branch's raw
source content already there" — the latter was never the question this decision meant
to ask, just an oversight in translating "content-based, not oid-ancestry-based" into
code.

Regression test:
`run_does_not_resurrect_a_mirror_only_branch_merged_except_for_excluded_paths`
(`src/commands/sync.rs`) — same fixture shape as
`run_does_not_resurrect_a_mirror_only_branch_already_merged_and_deleted_on_dest`, with
`.gitprismignore` on `main` excluding `secret.txt` and `feature-x`'s own commit
touching both `feature.txt` and `secret.txt` together. Confirmed to fail against the
pre-fix code (third sync recreated `feature-x` on dest) and pass once the fix landed.
