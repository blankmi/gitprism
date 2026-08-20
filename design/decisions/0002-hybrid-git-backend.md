---
type: Decision
title: Hybrid git backend — git2-rs for local object/merge work, real git subprocess for push/fetch
description: gitprism talks to git through two channels rather than one, split by whether the operation touches a remote.
tags: [architecture, git-library]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

Four candidates were considered for talking to git from Rust: `git2-rs` (libgit2
bindings), `gix`/gitoxide (pure Rust), shelling out to the `git` CLI entirely, or a
hybrid split by operation type.

[Jujutsu](../references/jujutsu.md) — the most directly comparable current (2026)
Rust tool, in that it's a client-side program that both rewrites git objects locally
*and* has to push/fetch against real remotes with real auth — already answers this
question in production: gix for local object-graph work, but push and fetch go
through a real `git` subprocess. Their stated reason is libgit2's long-standing weak
SSH support (20+ open issues); routing push/fetch through the actual `git` binary
means inheriting its credential helpers, SSH agent, and `GIT_ASKPASS` handling for
free.

[josh](../references/josh.md) — the closest prior art for the filtering problem
itself — depends on `git2` plus several low-level `gix` crates, and its maintainers
found full gix integration "somewhat difficult," especially async. That friction is
tied to josh-proxy being an async server implementing the git protocol itself; it
doesn't obviously transfer to a batch/CLI tool like gitprism.

# Decision

Split by operation, same shape as jj:

* **Local object-graph work — including the dest→source merge — goes through
  `git2-rs`.** Not `gix`: gitoxide's own documentation currently lists full merge
  workflows as still under development, and the dest→source direction of this project
  *is* a merge — the single hardest, most load-bearing operation here. `git2-rs`'s
  merge/index/tree API is old and complete.
* **Push and fetch go through a real `git` subprocess**, not through `git2-rs`'s own
  push/fetch. This is the fast-forward-only push path (the entire reason
  `git-filter-repo` was ruled out — see
  [references/git-filter-repo](../references/git-filter-repo.md)), so it should use
  the same networking/auth path a human running `git push` would, rather than a
  library reimplementation of it.

# Why not gix for the local half

jj can accept gix's current merge gap because it has its own conflict
representation and doesn't lean on gix to do a traditional 3-way merge the way this
project's dest→source sync needs to. We don't have that option — merge is core, not
incidental — so we take the library with the complete, proven implementation instead
of the "more idiomatic pure-Rust" one.

# Consequences

* Two git-access code paths to reason about (library calls vs. subprocess calls)
  instead of one. This is accepted complexity, not overlooked complexity — see the
  "hybrid" option's cost noted during the discussion.
* Push/fetch subprocess calls mean parsing `git`'s text/porcelain output where needed,
  same as [git-subtree](../references/git-subtree.md) already does.
* If `git2-rs`'s libgit2 dependency ever becomes a real problem (e.g. the pending
  libgit2 v2.0 ABI break), the local half can be revisited independently of the
  push/fetch half, since they're already separated.

## Implementation-hardening addendum (2026-08-20)

Remote subprocesses keep the hybrid boundary but treat every remote and branch
as data: branch names are validated before any Git subprocess or repository
mutation, remote operands beginning with `-` are rejected, and Git's `--`
terminator is placed before the remote operand. Fetch constructs only the
fully-qualified `refs/heads/<branch>` source ref; callers cannot supply a raw
refspec. Network operations disable interactive prompting and frame, redact,
bound, and escape captured diagnostics. Push retry classification consumes
Git's `--porcelain` ref-status records rather than localized prose.
