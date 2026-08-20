---
type: Decision
title: Ship a `gitprism resolve` helper for human conflict resolution
description: Rather than documenting a manual fetch/cherry-pick/trailer procedure, gitprism provides a subcommand that reproduces the conflict, hands off to normal git conflict-resolution UX, and closes the trailer bookkeeping loop itself.
tags: [architecture, safety, conflict-handling, ux]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

Given the hard-stop conflict policy
([decisions/0007](0007-conflict-policy-hard-stop.md)), a human has to actually resolve
the conflict from outside the pipeline run that hit it. Because setup grafted real
shared history ([decisions/0006](0006-setup-uses-real-shared-history.md)), the
conflict is reproducible with plain `git fetch`/`git cherry-pick` by anyone with
access to both remotes — no gitprism-specific state is needed to reproduce it.

What's missing if left fully manual: closing the loop correctly depends on the
resulting commit carrying an exact `Gitprism-Dest-Commit: <dest-sha>` trailer
([decisions/0003](0003-mapping-state-in-commit-trailers.md)). A human hand-writing
that trailer mid-resolution, under time pressure, is a plausible place for a typo or
omission to slip in — and a wrong or missing trailer breaks resume/loop-prevention
*silently*, not loudly, which is the worst time for a mistake to hide in a tool whose
whole premise is never silently corrupting history.

# Decision

Ship `gitprism resolve <pair>` (or equivalent): it performs the fetch and starts the
cherry-pick for the human, then hands off entirely to git's own normal
conflict-resolution flow (edit files, `git add`, and gitprism wraps
`cherry-pick --continue`). Once the human finishes, gitprism appends the correct
trailer and performs the ff-only push itself.

# Why

* Keeps the actual conflict resolution 100% standard git — no custom merge UI, no new
  workflow for the human to learn beyond running one command first.
* Removes the one manual step that's genuinely risky (hand-typing a machine-parsed
  trailer) without adding any new resolution mechanism to trust.
* The project owner confirmed this scenario is expected to be rare in normal use but
  must still be prepared for — a helper that makes the rare path safe and low-friction
  is worth the extra surface for exactly that reason.

# Consequences

* One more subcommand to design and maintain beyond the core sync logic.
* The helper needs its own small piece of design later: how it identifies "which
  pair, which conflicting commit" without the human having to supply raw shas by
  hand — deferred as a concrete follow-up, not resolved here.

## Source-to-dest resolution

[Decision 0027](0027-source-to-dest-resolution-state.md) extends this helper to
source→dest conflicts using an authenticated linked worktree and filtered
synthetic patch. The original source checkout remains untouched.
