---
type: Decision
title: Mutating commands serialize per Git common directory and advance local refs with CAS
description: Every mutating gitprism command takes a non-blocking common-directory lock, and dest-to-source local advancement uses a preflight plus compare-and-swap update.
tags: [architecture, concurrency, safety, git]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

`setup`, `sync`, and both `resolve` directions can fetch, invoke Git, update
refs, or change a worktree. A second gitprism process operating on the same
repository can invalidate the first process's ancestry and checkout checks.
Linked worktrees also share the same refs and object database, so a lock tied
only to one worktree would not serialize the repository operation.

`sync`'s dest-to-source phase pushes the newly built source commit before
bringing this checkout's local branch up to the pushed tip. A dirty checked-out
branch can fail that local checkout after the remote has already advanced, and
a concurrent ref move can otherwise be overwritten by a forced local update.

# Decision

Each mutating command acquires an exclusive, non-blocking lock at
`<git-common-directory>/gitprism.lock` after repository discovery, state-key
validation, and protected-policy validation, but before any Git subprocess,
network operation, ref update, or worktree mutation. The lock file is opened
read/write/create without truncation. The operating system releases the lock
when the command drops its file handle. `policy-hash` is read-only and does not
take this lock.

Before a dest-to-source push, gitprism records the exact current local branch
OID, verifies that the built tip is a fast-forward of it, rejects tracked
working-tree or index changes when that branch is checked out, rejects an
untracked or ignored path only when the target would overwrite it, and
dry-runs the safe checkout. After the remote accepts the push, gitprism materializes the tree and
updates the local ref with `reference_matching` against the recorded OID. A
concurrent ref move is never forced over; because the remote push already
succeeded, the error reports that the local ref/worktree may need manual
reconciliation.

# Why

The common-directory lock prevents overlapping gitprism operations, while the
compare-and-swap remains necessary because ordinary Git commands and other
processes are outside that lock. Preflighting before the push prevents a
predictable dirty-checkout failure from leaving the remote ahead of the local
checkout. A non-blocking failure is explicit and retryable rather than hiding a
long-running operation behind a wait.

# Consequences

* Concurrent gitprism commands for linked worktrees fail immediately until the
  first operation finishes.
* The lock does not claim or prevent ordinary Git operations; CAS remains the
  final protection against overwriting an external ref move.
* A concurrent external move after the remote push can still require manual
  reconciliation, but gitprism will not force or reset the local branch.
* Rust 1.89 or newer is required for `std::fs::File::try_lock`.
