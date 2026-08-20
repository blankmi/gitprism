---
type: Decision
title: Source-to-dest conflicts use an authenticated linked-worktree operation
description: Source-to-dest conflicts are reproduced as a filtered synthetic patch in an isolated linked worktree and resumed from an authenticated Git ref.
tags: [architecture, safety, conflict-handling, filtering]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

Decision 0015 shipped `gitprism resolve` for dest→source cherry-picks, but
source→dest uses filtered three-way trees and can also hard-stop on a conflict.
The operator needs ordinary Git conflict markers without checking source-only
files out or mutating the source checkout.

# Decision

`gitprism resolve <branch> --direction source-to-dest` creates an authenticated
operation ref under `refs/gitprism/resolve/source-to-dest/<branch>`. The ref points
to a state commit whose canonical HMAC body records the source tip, fetched dest
tip, destination base, checkout commit, filtered patch commit, policy digest, and
whether the destination ref existed. The patch is a synthetic one-parent commit
whose parent and tree are the exact filtered first-parent/source trees used by
`sync`'s `merge-tree` path.

The patch is applied with `git cherry-pick --no-commit` in a unique linked
worktree checked out at the authenticated state commit whose tree is the
destination base. The source checkout is not changed, and
the temporary tree contains no source policy or excluded source content.
On a conflict, gitprism records the authenticated patch OID in that linked
worktree's `CHERRY_PICK_HEAD`; continuation requires that marker to remain
present and unchanged. The operator edits and stages ordinary conflict
markers, then runs:

```text
gitprism resolve <branch> --direction source-to-dest --continue
```

Continuation verifies the authenticated state, source tip, policy digest,
synthetic patch shape, worktree checkout, and an active `CHERRY_PICK_HEAD`
matching the authenticated patch. Edits to excluded paths are rejected. The
resolved index tree is then used directly as the destination tree: it already
contains the unchanged destination base, including destination-owned paths
that match source exclusions, while the filtered patch contains no excluded
source content. The authenticated destination commit is pushed
fast-forward-only; operation state remains when push or resolution fails, and
is removed only after a successful push and worktree cleanup. If the
destination moves before that push, the operator must preserve the staged
resolution, remove the linked worktree, start a new resolution against the
new destination, and reapply it.

# Why

The linked worktree gives the human standard Git conflict UX while preserving the
source checkout and its source-only files. Git refs and authenticated commit
metadata provide resumable state without a new application state file. Reusing
the existing filtered tree construction and `merge-tree` keeps conflict behavior
consistent with automatic sync.

# Consequences

The `resolve` direction defaults to dest→source for compatibility. Source→dest
resolution is only meaningful when the destination branch already exists; a
mirror-only branch must first be created by `sync`. A human resolution that
changes an excluded path must be restored before continuation rather than being
silently discarded. An empty allowed resolution still creates an authenticated
mapping commit so the same source commit is not retried forever.
