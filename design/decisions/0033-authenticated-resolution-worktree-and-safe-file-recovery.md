---
type: Decision
title: Authenticate resolution worktrees and recover files without path races
description: Source-to-dest continuation signs its exact generated worktree path and validates Git registration before access; bounded reads and setup recovery use opened-handle and create-new file operations.
tags: [security, filesystem, conflict-handling, worktrees]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

Source-to-dest conflict resolution persists an authenticated operation state
while an operator works in a linked worktree. Git's `.git/worktrees/*/gitdir`
metadata identifies that worktree, but its contents are filesystem input and
must not be allowed to redirect continuation into an unrelated repository.
Control-file reads and setup rollback also need to remain safe if a local
process races the command with symlink or hard-link replacements.

# Decision

The signed source-to-dest operation state includes a hex-encoded locator for
the exact generated resolution worktree path. New worktree paths are derived
from a canonical temporary-directory root and reserved with `create_dir`; a
collision fails rather than selecting a different path after the state is
signed. Continuation decodes the signed path, rejects path substitution and
symlink metadata, requires a regular linked-worktree `.git` file, and first
matches that exact path against a registered `gitdir` entry in this repository's
common directory. Only then is the worktree repository opened. Its common
directory and authenticated HEAD must match the operation state. Legacy state
without the locator fails closed.

Bounded file reads open the handle first, then validate handle metadata against
the path metadata; Unix additionally compares device/inode identity and rejects
hard links. Setup rollback removes the path entry and creates the replacement
with `create_new`, writing bytes and permissions through the opened handle, so a
raced symlink or hard link is refused rather than followed.

Conflict behavior remains unchanged: synchronization fails fast and leaves
content selection to the operator. These checks never auto-resolve or skip a
conflict.

# Why

The operation state's HMAC already authenticates its fields, so signing the
generated locator prevents repository metadata from substituting an arbitrary
path. Registration and common-directory checks ensure that the path is not
merely an existing repository but the linked worktree created for this
operation. Open-handle identity checks close the check-then-open race on Unix;
`create_new` gives portable no-follow behavior for restoration because any
replacement at the path makes creation fail.

# Consequences

An incomplete or tampered in-progress source-to-dest operation must be
restarted or cleaned up manually. A legacy operation state created before this
decision cannot be continued by gitprism. Operators receive a clear error
instead of risking access to a path outside the intended repository.
