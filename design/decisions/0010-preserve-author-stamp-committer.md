---
type: Decision
title: Preserve original author identity; gitprism stamps itself as committer
description: Every commit gitprism creates keeps the original commit's author name/email/date, but is committed under gitprism's own identity at sync time.
tags: [architecture, identity, commits]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

Every commit git tracks has two identities: **author** (who wrote the change) and
**committer** (who is creating this commit object). gitprism creates new commit
objects in both directions — filtered commits on dest, cherry-picked commits on
source — so both fields need a policy. The project owner's answer was "preserve."

# Decision

**Preserve the original author** (name, email, authored-date) exactly, on every
commit gitprism creates. **Set the committer to gitprism's own identity**, timestamped
at sync time — not the original commit's committer.

This is not a partial reading of "preserve" — it's git's own standard convention for
exactly this kind of operation. `git cherry-pick`, `git rebase`, and
`git filter-branch`/`git-subtree split` all keep the original author while updating
the committer to whoever performed the rewrite, at the time they performed it. A
commit that's been mechanically rewritten and moved to a new position in history
*is* committed by the tool that moved it, even though it was authored by someone
else — preserving both fields identically would misrepresent that gitprism did the
rewriting, not the original author.

# Why

* Matches existing git tooling conventions exactly — nothing novel for anyone reading
  `git log` to learn.
* Preserves attribution/blame correctly: the person who wrote the change is still
  correctly credited as author.
* Makes mechanically-created commits identifiable as such (committer = gitprism, not
  a human) without needing to inspect trailers.

# Consequences

* gitprism needs its own committer identity (name/email) configured wherever it runs
  — a concrete config detail for later, not a design fork.
* If either repo enforces signed commits, gitprism's own commits need to be signable
  under its committer identity — flagged here as a real operational requirement to
  check, not resolved.
