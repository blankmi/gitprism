---
type: Decision
title: Exclude-list uses .gitignore syntax, in a self-excluding .gitprismignore file
description: The versioned exclude-list from decisions/0004 is a single dotfile named .gitprismignore at source's root, using exactly .gitignore's pattern syntax, and it excludes itself automatically.
tags: [architecture, filtering, config]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

[decisions/0004](0004-exclude-list-versioned-in-source.md) settled that the
exclude-list is a versioned file committed inside source, format left open. The
project owner's answer: "configuration similar to `.gitignore` or so."

# Decision

* **Filename**: `.gitprismignore`, at source's repo root — same naming convention as
  `.gitignore`, immediately recognizable, no new mental model.
* **Syntax**: exactly `.gitignore`'s pattern syntax (globs, `#` comments, `!`
  negation, trailing-`/` for directories) — not a custom DSL. There's no reason to
  invent new pattern syntax when a well-known one already exists and is directly
  reusable; `git2-rs`/the `ignore` crate already implement `.gitignore`-semantics
  matching correctly, so this is also an implementation win, not just a familiarity
  one.
* **Self-exclusion is automatic**: `.gitprismignore` itself is always treated as
  excluded, without needing to be listed inside itself. This closes the gap
  [decisions/0004](0004-exclude-list-versioned-in-source.md) flagged (the exclude-list
  file is source-only metadata) without relying on someone remembering to list it.

# Why

* Reuses a pattern language every git user already knows, instead of asking anyone
  reading `.gitprismignore` to learn gitprism-specific syntax.
* Reuses an existing, correct implementation of that syntax rather than hand-rolling
  glob matching — consistent with this project's recurring principle of not
  rebuilding what already exists correctly (see
  [decisions/0005](0005-branch-pairs-are-a-configured-list.md),
  [decisions/0006](0006-setup-uses-real-shared-history.md)).
* Automatic self-exclusion removes a foot-gun (forgetting to exclude your own filter
  file, leaking it to dest) rather than documenting around it.

# Consequences

* Any future gitprism control files (e.g. if [decisions/0008](0008-ship-resolve-helper.md)'s
  resolve helper needs its own state later) should follow the same
  automatically-excluded convention rather than requiring manual listing.
