---
type: Decision
title: Bound repository-controlled data before expensive processing
description: Static process limits bound control-file reads, commit messages, tree traversal, branch collections, conflict reporting, pending history, and marker scans; exceeding a limit fails the operation without guessing or dropping data.
tags: [security, reliability, git, resource-limits]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

The repository is an untrusted input boundary. A malicious or merely
unexpected checkout can contain huge control files, commit messages, histories
with many pending commits, millions of tree entries, very deep trees, or a
large conflict index. Letting those values allocate or recurse without a
bound can exhaust memory or stack space before gitprism reaches its normal
fail-fast conflict policy.

Earlier decision 0019 deliberately avoided hiding the graft point during
first-parent marker scans. That correctness rule remains, but “do not hide”
does not require an unbounded process. Decision 0032 supersedes the earlier
wording that described marker scans as unbounded: scans inspect the complete
first-parent line up to a static safety budget and fail clearly if it is
exceeded.

# Decision

All repository-controlled regular-file reads for `.gitprism.toml`,
`.gitprismignore`, setup rollback control files, linked-worktree metadata, and
`CHERRY_PICK_HEAD` use a bounded reader. `symlink_metadata` must identify a
regular non-symlink file before reading, and the file length is checked before
the first allocation. Growth during the read is checked again. Control files
are limited to 1 MiB and small Git state files to 64 KiB.

Commit messages are limited to 1 MiB before UTF-8 conversion or marker parsing.
Configured branch lists are limited to 1,024 entries; source branch discovery
is limited to 4,096 branches. Pending history is limited to 10,000 commits per
operation, and each first-parent marker scan is limited to 100,000 commits.
Recursive Git tree filtering and checkout collision scanning share a traversal
budget of 1,000,000 entries and a maximum recursion depth of 256. Conflict
reporting is limited to 100,000 records and 8 MiB of raw path bytes.

These are static application limits. No repository-controlled file or policy
field can raise them. Exceeding a limit returns a contextual error. Sync still
stops on a real conflict and leaves the operator to resolve it; bounds never
select a side, skip a conflict, truncate data, or auto-resolve.

# Why

The limits are high enough for ordinary CI repositories while making the
maximum work and allocation explicit. Checking metadata before allocation
prevents a hostile length from reserving memory, while the second read check
covers a file that grows concurrently. Keeping the marker scan on the full
first-parent line preserves the safety invariant from decisions 0019 without
allowing an attacker to force an indefinitely expensive scan.

# Consequences

Very large repositories may need operational partitioning or manual Git work
before gitprism can process them. Operators receive a failure naming the
exceeded budget and can resolve the repository state outside gitprism. Tests
cover oversized regular files, configuration branch limits, marker-message
limits, and traversal depth/entry boundaries; normal repository and conflict
behavior remains covered by the existing integration suite.
