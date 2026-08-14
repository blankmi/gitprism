---
type: Decision
title: source→dest filtering becomes diff-based, superseding decisions/0009's "no merge semantics"
description: Each pending source commit is applied to dest as its own filtered diff against dest's current tip, not a full-tree snapshot replace — required for correctness once dest→source can land independent content between two not-yet-pushed source commits. This reintroduces real conflict risk on source→dest, hard-stopped the same way decisions/0007 already handles dest→source.
tags: [architecture, filtering, conflict-handling]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
---

# Context

Implementing dest→source surfaced a real bug in source→dest's existing filtering:
each pending source commit was applied to dest as a **full filtered snapshot** of
that commit's entire tree (`build_pending_dest_tip`/`filter_tree`), replacing dest's
whole tree wholesale. That was always safe *before* dest→source existed, because
source was guaranteed a strict superset of dest — no source commit's snapshot could
ever lack content dest legitimately had.

Once dest→source cherry-picks an independent dest commit onto source's tip
(decisions/0013's implementation), that invariant breaks: any older, not-yet-pushed
source commit that predates the cherry-pick in source's own history has a tree that
genuinely doesn't know about dest's independent content yet. Pushing that older
commit's full snapshot to dest would silently regress dest's own content back out —
exactly the silent-data-loss failure mode the project's no-force-push requirement
exists to prevent (see [references/git-filter-repo](../references/git-filter-repo.md)).
This isn't a rare interleaving: dest receiving an independent PR merge while source
separately has its own unpushed commits is the normal operating condition this tool
is built for ([playbooks/0001](../playbooks/0001-gitlab-pipeline-triggers.md)
explicitly combines independent triggers for exactly this reason).

This directly collides with [decisions/0009](0009-push-race-refetch-and-recompute.md),
which states source→dest was "deliberately designed to have no merge semantics at
all," specifically to avoid ever needing conflict-handling on that side.

# Decision

Source→dest now applies each pending source commit as its **own diff** (against its
immediate parent, mainline for merge commits), filtered, onto dest's current growing
chain tip — via `git2`'s diff + `apply_to_tree` with a per-delta filtering callback
that drops any delta touching an excluded path before application, rather than
filtering after the fact. This is symmetric to how dest→source already applies each
pending dest commit as a cherry-pick.

Consequence accepted explicitly: source→dest **can now genuinely conflict** — dest
and source having each independently changed the same shared content differently,
the same real-conflict shape [decisions/0007](0007-conflict-policy-hard-stop.md)
already describes for dest→source, just previously assumed one-directional. It gets
the identical treatment: hard-stop, no auto-resolve, no skip; whatever applied
cleanly before the conflict is still pushed; the human resolves it by hand (the
project owner's own framing: "if dest→source is not possible for this branch a human
operator has to do it by hand" — this is that same operational reality, now
symmetric). [decisions/0008](0008-ship-resolve-helper.md)'s `gitprism resolve` helper
is the mechanism for acting on it from outside a pipeline run, regardless of which
direction hit it.

This supersedes decisions/0009's "no merge semantics at all" framing for source→dest
specifically — 0009's actual subject (refetch-and-recompute beats rebase for a lost
fast-forward *race*) still stands unchanged; only its incidental claim that
source→dest has no merge semantics no longer holds.

# Why

* The regression is real and not an edge case — it's the normal case once both
  directions actively run against actively-developed repos. A conflict-free
  filtering model that silently drops content isn't actually safer than one that can
  conflict and stops.
* Diff-based filtering is provably equivalent to the old full-snapshot approach for
  any purely linear history (every existing source→dest test), since diffing a
  commit against its own parent and applying that onto the correspondingly-filtered
  parent produces the same resulting tree as filtering the commit's full snapshot,
  whenever nothing foreign is in the mix. Nothing about already-settled, single-
  direction behavior changes.
* Reuses the same conflict-handling shape decisions/0007 and 0008 already built for
  dest→source, rather than inventing a second one — one hard-stop policy, one
  resolve helper, both directions.
* Filtering *before* applying the diff (skip excluded deltas outright) rather than
  filtering the merged result afterward is required, not stylistic: an already-
  excluded path's own history (e.g. `.gitprismignore` being edited repeatedly) would
  otherwise look like a modify/delete conflict on every single sync, since dest never
  has that path at all to merge against.

# Consequences

* `build_pending_dest_tip` no longer calls a whole-tree `filter_tree`; it diffs each
  pending source commit against its own parent and applies the filtered diff onto the
  growing dest-chain tip.
* Source→dest gains the same "push what applied cleanly, stop at the conflict, leave
  no trailer for the unresolved commit" behavior dest→source has, and the same
  `gitprism resolve` closes the loop for either direction.
* A future reader of decisions/0009 should read its "no merge semantics" framing as
  historical context for *why the race-recompute question came up the way it did*,
  not as a currently-true property of source→dest.
