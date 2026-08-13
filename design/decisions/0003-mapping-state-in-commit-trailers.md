---
type: Decision
title: Commit-mapping state lives in commit-message trailers, not notes/db/state-file
description: gitprism records the source<->dest commit correspondence as a trailer on every commit it creates, and uses the same trailer to both resume syncing and prevent re-import/re-export loops.
tags: [architecture, state, mapping]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

The tool needs, for both sync directions: (a) where to resume from, and (b) a way to
avoid re-importing/re-exporting a commit it already synced (which would otherwise
loop forever between the two repos). Four options were considered: commit-message
trailers, git notes, a committed state file, and an external sidecar database.

Two independent existing tools were checked and both converged on the same answer:
[Copybara](../references/copybara.md) stamps every destination commit with a
`GitOrigin-RevId: <origin-sha>` label and resumes by scanning history for the last
one; [git-subtree](../references/git-subtree.md) does the structurally identical
thing with `git-subtree-split`/`git-subtree-mainline` trailers on its rejoin commits.
[josh](../references/josh.md) instead uses git notes plus a local cache — a different
answer, but for a different requirement (O(1) lookup at proxy-serving scale across
potentially many virtual repos), which this project doesn't have.

# Decision

Every commit gitprism creates carries a trailer pointing at its counterpart:

* Commits pushed to **dest** (source→dest direction) get
  `Gitprism-Source-Commit: <source-sha>`.
* Commits created on **source** (dest→source direction) get
  `Gitprism-Dest-Commit: <dest-sha>`.

Resuming a sync means scanning the relevant branch's history for the most recent
commit carrying the relevant trailer. Loop prevention is the same mechanism read the
other way: when scanning dest for commits to reflect back to source, any commit
already carrying `Gitprism-Source-Commit` is recognized as gitprism's own prior
output and skipped; symmetrically for source commits carrying
`Gitprism-Dest-Commit` when scanning for what to push to dest.

# Why

* No extra ref to keep in sync (unlike git notes, which need `refs/notes/...` fetched
  and pushed on top of the branch itself — one more thing that can fall out of sync
  or get stripped by a host/mirror that doesn't forward arbitrary refs).
* No separate state file to keep consistent with what history actually says happened;
  the state *is* the history, so it can't drift from it.
* One mechanism serves both jobs (resume-point and loop-prevention) instead of two.
* Directly precedented by two independently-arrived-at real tools.

# Consequences

* gitprism modifies the commit messages of commits *it authors* (adding a trailer at
  creation time) — it never edits a message on a commit it didn't create. Filtered
  source→dest commits and merged-back dest→source commits are already new commit
  objects (different tree/parents than their origin), so adding a trailer costs
  nothing conceptually.
* Resuming requires a history scan each run. Accepted for now; a cheap local
  cursor/ref cache can be layered on top later purely as a speed optimization, without
  changing the trailer as the source of truth (the same relationship josh's sled cache
  has to its git-notes source of truth).
* Empty commits that get pruned after filtering (see
  [requirements/0001](../requirements/0001-workflow-and-scope.md)) produce no dest
  commit and therefore no trailer for that specific source commit. This is harmless:
  the next run's history scan simply re-examines those already-empty source commits
  and finds them still empty. Correctness holds; the only cost is a bit of redundant
  re-checking, bounded by how often sync runs.
