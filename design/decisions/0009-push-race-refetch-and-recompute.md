---
type: Decision
title: A lost ff-only push race is handled by refetch-and-recompute, not rebase
description: If dest's tip moves between fetch and push, gitprism discards the locally-built commits and redoes the filter step against dest's new state, rather than rebasing what it already built.
tags: [architecture, push, concurrency]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

The source→dest push ([requirements/0001](../requirements/0001-workflow-and-scope.md))
is fast-forward-only. If dest's tip advances between gitprism's fetch and its push
attempt, the push fails on a race, not a content conflict — this is a different
failure than the one handled in
[decisions/0007](0007-conflict-policy-hard-stop.md). Two options were considered:
refetch dest and recompute the filtered commits from scratch, or rebase the
already-built commit objects onto dest's new tip.

# Decision

Refetch and recompute from scratch, with a bounded number of retries.

# Why

* The recomputed commits are guaranteed correct because they're built directly
  against dest's actual current state, not a guess about how to reparent stale ones.
* Reuses the exact same filtering/build logic as the normal case — no separate
  "race recovery" path to maintain.
* Rebasing is itself a merge-shaped operation and can conflict, which would
  reintroduce a conflict-handling question on the one direction
  ([decisions/0006](0006-setup-uses-real-shared-history.md)) that was deliberately
  designed to have no merge semantics at all. Recompute avoids that entirely.
* Consistent with the project's recurring principle so far: don't hand-build a
  substitute for something a plain, already-correct mechanism handles for free (see
  [decisions/0005](0005-branch-pairs-are-a-configured-list.md) and
  [decisions/0006](0006-setup-uses-real-shared-history.md) for the earlier instances
  of this).

# Consequences

* Under contention, some filtering work is redone. Accepted: this tool runs as an
  occasional pipeline job, not a high-frequency service, so the cost is negligible.
* A bounded retry count is needed so a pathologically contended dest (constant pushes
  from elsewhere) fails loudly eventually rather than retrying forever — exact bound
  is an implementation detail, not a design fork.
