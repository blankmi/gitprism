---
type: Decision
title: Setup grafts source's history onto dest's real commits, not a snapshot
description: source's initial commit is a real child of dest's tip, giving every future source<->dest merge a native git merge-base instead of a hand-rolled substitute.
tags: [architecture, setup, merge-base]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

"Copy everything from destination into source" (the setup step in
[requirements/0001](../requirements/0001-workflow-and-scope.md)) can mean either: (a)
source's first commit is a real child of dest's actual tip commit — the same shape as
[git-subtree](../references/git-subtree.md)'s `add`, which fetches and merges in a
whole external history — or (b) an orphan commit whose tree merely matches dest's
content, with no real parent link.

The choice determines whether the dest→source merge (the hardest remaining mechanism
in this project) can rely on git's native `merge-base`/cherry-pick/rebase machinery,
or has to reconstruct cross-repo ancestry by hand.

# Decision

(a). source's initial commit has dest's real tip commit as its git parent. Dest's
full history becomes literally reachable as ancestors of source's history, in the
same object graph (after fetching dest as a remote into source's local object
database, a mechanical precondition for referencing its commits at all).

# Why

* Every future merge, cherry-pick, or rebase between a source commit and a dest
  commit gets a real common ancestor for free, because one genuinely exists in the
  graph. [decisions/0002](0002-hybrid-git-backend.md) chose `git2-rs` specifically for
  its complete merge/index API — that choice only pays off if there's a real base for
  it to merge against.
* The alternative means no real common ancestor ever exists between the two
  histories, which would force us to reimplement cross-repo ancestry resolution by
  hand — the same category of mistake corrected in
  [decisions/0005](0005-branch-pairs-are-a-configured-list.md) (don't build a
  substitute for something git already does natively).
* Directly precedented by `git subtree add`.

# Consequences

* source's `git log` shows dest's pre-existing commits as real ancestors, not just
  source-authored ones. This is what real shared ancestry looks like, not a defect to
  suppress.
* Setup requires fetching dest into source's local object database before source's
  first commit can be created — a mechanical step, not a design choice with
  alternatives; any cross-repo git operation needs the other side's objects available
  locally regardless of this decision.
