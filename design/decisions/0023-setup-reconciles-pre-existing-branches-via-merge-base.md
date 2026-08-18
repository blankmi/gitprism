---
type: Decision
title: Setup reconciles any pre-existing branch via merge-base, hard-failing when none exists
description: decisions/0021's oid-equality precondition generalizes to a real merge — a pre-existing local branch is reconciled with dest via git's own merge-base plus the existing merge_tree primitive (0016), producing a two-parent commit when real shared history exists; a branch with no merge-base at all (genuinely unrelated history) hard-fails unconditionally, with no flag or override to force it through gitprism itself.
tags: [architecture, setup, bootstrap, conflict-resolution]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

Running `gitprism setup` against `~/work/catenax-connector` — a real GitLab project
with its own independent commit history on `main`, plus unrelated local branches
`ai-setup` and `backup` — hit [decisions/0021](0021-setup-accepts-a-clean-clone-of-dest.md)'s
hard-fail exactly as designed: `main`'s tip doesn't match dest's fetched tip, so it's
rejected as unrelated history. That's the correct behavior for 0021's narrower
question ("is this repo just an unmodified clone of dest?"), but it leaves no
supported path at all for the case this repo actually is: a pre-existing project that
is genuinely meant to *become* source, carrying real history dest has never seen.

The first draft of this decision (recorded in `design/log.md` but superseded here)
proposed a `--adopt` flag: explicit opt-in, mirroring git's own
`--allow-unrelated-histories`, gating a real two-parent merge commit. Working through
it in conversation surfaced a sharper question: why does "empty repo," "clean clone
of dest" (0021), and "real independent history" need three different mechanisms at
all?

Looking at what `graft_branch` actually does answers that. It creates a commit whose
parent is dest's fetched tip and asks libgit2 to move the branch ref there;
`git_commit_create` refuses that ref-update unless the ref's current target already
*is* that parent (or the ref doesn't exist yet). 0021 works by reusing `graft_branch`
completely unchanged specifically *because*, when local tip equals dest tip, that
safety check passes trivially — 0021 never needed a distinct "merge" mechanism, only
a precondition proving the trivial case actually applies. Empty and clean-clone
aren't two special cases; they're both the degenerate end of one general operation:
reconciling whatever's already on a branch with dest's tip.

Pushed to its natural conclusion, real independent history is just the non-degenerate
end of that same operation — not a reason to invent a second mechanism (and
`--adopt`'s explicit-opt-in flag was itself an uncomfortable fit: a flag whose entire
purpose is to bypass a safety check is a foot-gun once it exists, liable to get
reached for out of habit rather than genuine intent). [decisions/0016](0016-both-directions-merge-via-real-git-merge-tree.md)
already gives this codebase exactly the primitive a real reconciliation needs —
`git merge-tree --write-tree --merge-base=<base> <ours> <theirs>` — and `sync.rs`
already computes merge-base via git2's own `Repository::merge_base` in more than one
place (`graft_point`, `already_merged_into_a_landing_branch`). Real prior art for the
*edge* this raises — two histories with literally no shared ancestor — is git's own
`--allow-unrelated-histories` flag: proof that git itself treats "no merge-base"
as a distinct, deliberately-gated case, not something a 3-way merge algorithm should
paper over. Where this decision explicitly departs from that precedent, on direct
instruction: gitprism offers no equivalent override. If a local branch and dest
genuinely share no history, `setup` refuses outright, permanently, with no flag to
work around it — combining truly unrelated histories is something to do with real
git, deliberately, outside gitprism, before running `setup` at all.

# Decision

Generalize 0021's per-branch precondition from an oid-equality check to a
merge-base check, and let the actual reconciliation be a real merge when one is
possible:

1. **For each branch in `config.branches` that already exists locally**, after
   fetching dest's current tip for that name (today's existing fetch-everything-
   upfront pass): compute `repo.merge_base(local_tip, dest_tip)`.
2. **The local tip already carries a `Gitprism-Dest-Commit` trailer** (checked
   before the merge-base question below, since it always has an answer here — a
   prior graft's own parent *is* dest's old tip, so a merge-base always exists) →
   hard-fail unconditionally: this branch is setup's own prior output, and setup
   is a one-time step ([decisions/0006](0006-setup-uses-real-shared-history.md),
   [decisions/0012](0012-config-versioned-in-source.md)), never something to run
   again against its own graft. `gitprism sync` is the tool for picking up dest's
   newer commits from here, not a second `setup`. Caught during implementation
   review, not in the original draft of this decision: without this check, the
   merge-base generalization below would otherwise happily and silently merge
   dest's newer tip into setup's own prior graft on a second run, since that
   graft's parent already gives it a merge-base with dest — quietly turning
   "setup" into a second, undocumented sync mechanism.
3. **No merge-base exists** (git2 returns an error — the two histories share no
   common ancestor) → hard-fail immediately, before touching anything: *"source's
   local branch %s has no history in common with dest — gitprism won't merge
   unrelated histories automatically; merge dest into it yourself with real git
   first (e.g. `git merge --allow-unrelated-histories <dest-remote>/%s`), then
   re-run setup."* No flag exists to skip this check. This is unconditional and
   permanent, not a default that can be overridden.
4. **Local tip is identical to dest tip** (0021's original case: merge-base equals
   both) → skip merge machinery entirely, exactly as 0021 already does — a
   single-parent graft onto dest's tip. This avoids ever constructing a two-parent
   commit whose parents are the same commit, which is degenerate and unnecessary.
5. **A real merge-base exists and the tips differ** → reconcile via the existing
   `merge_tree(base, ours = local_tip, theirs = dest_tip)` primitive:
   - **Clean** → build a real two-parent commit: first parent is the local branch's
     own current tip (so `update_ref` still satisfies libgit2's
     first-parent-must-match-current-ref-target safety property, and so
     [decisions/0019](0019-marker-scans-are-first-parent-only.md)'s first-parent-only
     scans keep treating this branch's own history as primary going forward), second
     parent is dest's tip, tree is merge-tree's resulting tree with `.gitprism.toml`/
     `.gitprismignore` layered on top the same way today's plain graft layers them
     onto dest's tree.
   - **Conflict** → hard-stop exactly per [decisions/0007](0007-conflict-policy-hard-stop.md):
     name the conflicting paths, commit nothing, push nothing. No auto-resolution,
     no `-X ours`/`-X theirs`, consistent with every other conflict this tool
     refuses to guess through.
6. **A branch in `config.branches` with no local branch yet, or a genuinely empty
   repo**, is grafted exactly as today ([decisions/0006](0006-setup-uses-real-shared-history.md)) —
   no merge-base question even arises when there's no local tip to compare.
7. **Local branches not named in `config.branches`** (e.g. `ai-setup`, `backup`)
   remain entirely outside setup's view, as already decided: they're never
   inspected, never block a run, never get touched.

This supersedes 0021's mechanism, not just its precondition wording: 0021's
oid-equality check becomes case 4 above, a special (and now provably correct,
rather than separately-implemented) instance of the general merge-base rule.

# Why

* **One mechanism instead of three.** Empty repo, clean clone of dest, and real
  independent history stop being three cases needing three different code paths
  (plain graft / oid-equality precondition / adopt-flagged two-parent merge) and
  become one reconciliation rule with three points on a spectrum, all falling out of
  the same merge-base check and the same `merge_tree` call this codebase already
  has.
* **No foot-gun flag.** An explicit `--adopt` override exists for exactly one
  purpose — bypassing the safety check — which makes it something to reach for
  reflexively once a user has typed it once, in situations where it wasn't actually
  intended. Requiring the merge-base to be established with real git first means the
  human, not a flag, is the one who decided these two histories belong together.
* **Consistent with 0007's hard-stop-over-guessing philosophy**, extended one step
  further back: not just "don't guess who wins a conflict," but "don't guess whether
  two histories were even meant to be combined" — no merge-base is treated the same
  as an unresolvable conflict, not as something to merge through cleanly just because
  the trees happen not to overlap.
* **No new git primitive needed.** `Repository::merge_base` is already used
  elsewhere in this codebase (`sync.rs`'s `graft_point`,
  `already_merged_into_a_landing_branch`) for exactly this "do these share history"
  question; `merge_tree` is already 0016's shared mechanism for both sync
  directions. This decision only wires existing primitives into `setup`, rather
  than introducing a new one.
* **The re-run guard reuses the exact bookkeeping this codebase already trusts.**
  `Gitprism-Dest-Commit` ([decisions/0003](0003-mapping-state-in-commit-trailers.md))
  already means "this commit is one of gitprism's own outputs" everywhere else in
  the codebase; recognizing it here is the same test, not a new concept, and
  `sync.rs`'s existing `trailer_value` helper (made `pub(crate)`) is the one
  already used to read it back.

# Consequences

* **`setup.rs`'s branch-reconciliation logic changes from a boolean oid comparison
  to a merge-base lookup followed by a conditional `merge_tree` call.** 0021's
  `pre_existing_branches` oid map and fetch-loop shape are still the right
  scaffolding; the comparison inside that loop changes.
* **`graft_branch` needs a second path for the two-parent case** — building the
  merge commit from `merge_tree`'s resulting tree oid, with two parents, instead of
  always reusing dest's tree wholesale with one parent. The single-parent path
  (cases 3 and 5 above) stays exactly as it is today.
* **New hard-fail message and code path for "no merge-base exists"**, distinct from
  0021's existing "doesn't match dest's current tip" message — the two are different
  situations (divergent-but-related history isn't reachable today either, since only
  exact-match or no-history-yet were previously accepted, but the wording should be
  clear that "unrelated" and "diverged" aren't being conflated).
* **A third, distinct hard-fail message and code path for "this is setup's own
  prior output"**, checked before the merge-base question (point 2 above) — without
  it, this decision would have silently regressed 0021's own "a repo that already
  had `gitprism setup` run against it once still hard-fails... re-running setup
  remains unsupported" consequence, since a prior graft's parent always gives it a
  merge-base with dest. `sync.rs`'s `trailer_value` moves from private to
  `pub(crate)` so `setup.rs` can reuse it, rather than duplicating trailer parsing.
* **Rollback (`TouchedBranch`/`rollback_branches` from 0021) is unaffected in
  shape** — still original-oid-or-none per branch — but a pre-existing branch's
  "original oid" can now be a real independent tip rather than dest's own tip;
  resetting to it on a later branch's failure still correctly undoes an in-progress
  reconciliation.
* **Scoped to whatever's on the branch already**, same as 0021 — this never forces a
  checkout or moves `HEAD`; a currently-checked-out branch that gets reconciled still
  needs its working tree updated to match the new merge commit, the same way 0021's
  clean-clone case already does.
* **Not decided here**: the exact post-conflict recovery story for this one-time
  case. `gitprism resolve` (0008/0015) is built around dest→source's ongoing sync
  loop, not a one-time setup graft; today the answer is "finish it by hand with real
  git and re-run setup," which is left as an open follow-up rather than solved here.
* **Not decided here**: whether a merge-base existing but the two sides sharing zero
  overlapping file paths deserves its own message or treatment. Deliberately *not*
  special-cased — that's an ordinary clean `merge_tree` result under this decision,
  reconciled the same as any other non-conflicting merge, per the explicit choice to
  gate only on merge-base existing at all, not on how much the two trees overlap.
