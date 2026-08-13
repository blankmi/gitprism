---
type: Reference
title: Jujutsu (jj)
description: A Rust, git-compatible VCS. Relevant here specifically for its git-backend split — gix for local object work, a real git subprocess for push/fetch — which is the direct precedent for gitprism's hybrid decision.
resource: https://github.com/jj-vcs/jj
tags: [prior-art, rust, git, vcs]
sources:
  - { id: issue2316, resource: "https://github.com/jj-vcs/jj/issues/2316", title: "FR: Switch to gitoxide backend instead of libgit2" }
  - { id: issue5548, resource: "https://github.com/jj-vcs/jj/issues/5548", title: "libgit2 -> gitoxide migration tracking issue" }
  - { id: gitrs, resource: "https://github.com/jj-vcs/jj/blob/main/lib/src/git.rs", title: "jj-lib git.rs" }
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
status: stable
---

# What it is

A git-compatible version control tool, written in Rust, with a first-class git backend
(its only production-ready backend today).

# Why it's relevant here

Not for its filtering/sync features — jj doesn't do cross-repo filtering — but for a
concrete, current (2026) precedent on the exact question gitprism faced: which
library talks to git.

# What it actually does

* jj migrated its local backend from libgit2 (via git2-rs) to **gix** (gitoxide) for
  local repository/object-graph operations. libgit2 is now optional-feature-flagged
  and headed for removal.[^issue5548]
* jj performs **push and fetch by invoking the real `git` executable as a
  subprocess**, not through gix (or, previously, git2-rs). The migration discussion
  points at libgit2's long-standing SSH weaknesses (over 20 open issues) as a driver;
  going through the real `git` binary means inheriting its credential helpers, SSH
  agent, and `GIT_ASKPASS` handling for free instead of reimplementing them.[^issue2316]
  `jj-lib`'s subprocess options explicitly expose an `environment` field documented as
  being for `GIT_ASKPASS`/`GIT_TRACE`.[^gitrs]

# How this shaped gitprism's decision

This is the direct precedent for
[decisions/0002](../decisions/0002-hybrid-git-backend.md): local object/tree/merge
work through a library, push/fetch through the real `git` binary. The one place
gitprism's answer differs from jj's: for the *local* half, gitprism chose git2-rs over
gix, because gix's merge support is still incomplete
([references/josh](josh.md) and gitoxide's own docs both note work-in-progress merge
support) and the dest→source direction of this project is exactly a merge — the one
operation this project can least afford to build on a gap.
