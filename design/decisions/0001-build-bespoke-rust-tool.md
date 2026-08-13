---
type: Decision
title: Build a bespoke Rust sync tool, using josh as a design reference only
description: gitprism will not depend on or wrap josh; it will implement its own filtering and commit-mapping engine, scoped to exactly two repositories.
tags: [architecture, build-vs-reuse]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

josh ([references/josh](../references/josh.md)) already solves the hard part of this
problem — reversible, non-destructive history filtering with a persisted
commit-mapping cache, which is exactly the property needed to avoid the
`git-filter-repo` force-push problem
([references/git-filter-repo](../references/git-filter-repo.md)). Four options were
considered: depend on `josh-core` as a library, shell out to the `josh` CLI/proxy,
build on `git subtree`, or write a bespoke engine using josh purely as a reference.

# Decision

Bespoke Rust tool. josh's ideas (deterministic commit mapping, a persisted cache
instead of full recomputation, skip-empty-commits, fast-forward-only pushes) inform
the design, but no josh code or service is used.

# Why

* The project's own stated goal is to understand *how* this works, not just to have
  it work — a dependency that hides the mechanism behind someone else's crate or
  service works against that goal.
* josh is built for arbitrary composition across N virtual repos via an always-on
  proxy service. This project has exactly two repos and one exclude-list; adopting
  josh's general machinery means carrying unused generality and an extra operational
  service for a much narrower job.
* `josh-core`'s public API is shaped to serve `josh-proxy`, not necessarily to be
  embedded standalone — depending on it as a library risks fighting its abstractions.
* The reverse-merge mechanic for the dest→source direction (the hardest part of this
  whole project) was not confirmed from josh's docs. Reading the source instead of
  reimplementing from a documented spec would mean the design stays only as
  understood as osmosis from someone else's code — the opposite of the goal.

# Consequences

* We accept re-deriving things josh already hardened over years in production
  (merge-commit handling, renames, empty-tree edge cases). These will need explicit
  design decisions and tests of their own — tracked as they come up.
* Every mechanism in this tool should be explainable from this `design/` bundle
  without pointing at josh's source as the actual explanation.
