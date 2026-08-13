---
type: Decision
title: A real dest<->source content conflict hard-stops that pair's sync
description: gitprism never auto-resolves or skips a genuine conflict; it aborts cleanly and fails loudly, leaving resolution to a human.
tags: [architecture, safety, conflict-handling]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

The dest→source cherry-pick ([decisions/0006](0006-setup-uses-real-shared-history.md)
gives it a real merge-base) only actually conflicts when dest changed *and* source
independently changed the same shared content since they last agreed — expected to be
rare given source and dest are expected to touch largely disjoint areas in normal
use, but possible, and the tool must be prepared for it regardless of how rare.

Three options were considered: hard-stop and hand off to a human, auto-resolve with a
fixed side always winning, or skip the conflicting commit and continue past it.

# Decision

Hard stop. On a real conflict, gitprism runs the equivalent of `git cherry-pick
--abort`, pushes nothing, and fails the pipeline run loudly with the exact
reproduction commands (dest commit sha, source branch tip) in its output. No trailer
is written for the unresolved commit, so [decisions/0003](0003-mapping-state-in-commit-trailers.md)'s
resume-scan naturally retries it on the next run once it's resolved — no special-case
bookkeeping needed for "this one's blocked."

# Why

* Auto-resolving with a fixed side silently discards a real, intentional change on
  whichever side loses — the exact failure mode the project's own no-force-push
  requirement exists to prevent (see
  [references/git-filter-repo](../references/git-filter-repo.md)). Introducing it
  ourselves on the other direction would be the same mistake in reverse.
* Skipping and continuing risks processing later commits that depend on the skipped
  one, producing a source state that never legitimately existed on dest — and the
  skipped commit still can't be marked "synced" without losing it, so it needs
  retrying anyway. It mostly reduces to hard-stop plus new failure modes.
* Matches git's own default behavior for merge/cherry-pick conflicts — nothing novel
  to learn on top of ordinary git.

# Consequences

* Commits after the conflicting one, for that branch pair, stay blocked until it's
  resolved — order must be preserved since later commits may depend on it. Commits
  before it that already succeeded are still pushed; forward progress up to the
  conflict is retained.
* Notifying a human that a conflict happened (Slack, CI alerting, etc.) is explicitly
  out of scope for gitprism itself — its responsibility ends at failing clearly and
  actionably; surfacing that failure is the pipeline's job.
* See [decisions/0008](0008-ship-resolve-helper.md) for how a human actually acts on
  this from outside an interactive pipeline run.
