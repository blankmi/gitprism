---
type: Reference
title: git subtree
description: Built-in git command for splitting a subdirectory's history out as a standalone project and merging it back; git-native, no extra dependency.
resource: https://github.com/git/git/blob/master/contrib/subtree/git-subtree.sh
tags: [prior-art, git, subtree]
sources:
  - { id: manpage, resource: "https://manpages.debian.org/testing/git-man/git-subtree.1.en.html", title: "git-subtree(1)" }
  - { id: source, resource: "https://github.com/git/git/blob/master/contrib/subtree/git-subtree.sh", title: "git-subtree source" }
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
status: stable
---

# What it is

A `git` ships-in-the-box command. `git subtree split` extracts a synthetic history for
one subdirectory (only the commits that touched it, rewritten so that subdirectory's
contents sit at the repo root); `git subtree push`/`pull` do that split and then a
normal push/merge against another remote.[^manpage]

# Why it's relevant here

It's the oldest and simplest git-native answer to "keep two repos' overlapping content
in sync," and — unlike `git-filter-repo` — its push path is a normal, non-rewriting
push, so it doesn't force the force-push problem on you.[^manpage]

# How it works

* No external state: the split point is re-derived each run directly from commit
  history (there's no persisted mapping database), which is also its main weakness —
  `split` without `--rejoin` walks the *entire* relevant history every time.[^source]
* `--rejoin` merges the split result back into the parent history with a marker commit
  carrying **`git-subtree-dir: <path>`**, **`git-subtree-mainline: <sha>`**, and
  **`git-subtree-split: <sha>`** trailers — the same "state lives in a commit-message
  trailer, scan history to resume" pattern Copybara uses independently. This lets the
  next split start from there instead of from scratch, though it's coarser than a real
  mapping index (one marker commit, not a full commit-to-commit map).[^manpage]

# Relevant difference from our problem

Subtree's model is "one repo embeds another **at a subdirectory**." Our source repo
has dest's content sitting at its **root**, with excluded files alongside it — there's
no single subtree prefix to split on, only a set of paths to keep. Subtree's
split/merge primitives don't map cleanly onto an exclude-list at the root; we'd be
using it against the grain, essentially reimplementing a filter on top of it anyway.
