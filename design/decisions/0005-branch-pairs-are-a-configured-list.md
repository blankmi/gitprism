---
type: Decision
title: Branch pairs are a configured list, not a hardcoded singleton
description: gitprism syncs a configured list of {source_branch, dest_branch} pairs, each processed independently by the same logic used for a single pair.
tags: [architecture, scope, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

Initially framed as "single pair now, generalize later if needed," with N pairs
assumed to cost more: a trailer format needing a branch-pair identifier, to
disambiguate resume points across pairs.

That assumption didn't survive scrutiny. Resuming a sync means scanning a specific
branch's history (via git's normal ref-scoped history walk) for the newest commit
carrying the relevant trailer
([decisions/0003](0003-mapping-state-in-commit-trailers.md)). That walk is already
scoped to the branch being asked about — `release-2.0`'s history walk only sees
commits reachable from `release-2.0`, regardless of shared ancestry with `main`. The
trailer never needs to name which pair it belongs to; the ref you scan already
supplies that context.

# Decision

The tool takes a configured list of `{source_branch, dest_branch}` pairs. Each pair
is synced by the exact same logic used for a single pair, independently. No change to
the trailer format in [decisions/0003](0003-mapping-state-in-commit-trailers.md).

# Why

Once the (mistaken) disambiguation cost is off the table, there's no real cost
difference between "supports 1 pair" and "supports N pairs" — it's a config-shape
choice (a list vs. a hardcoded pair), not an algorithm choice. With no cost gap,
there's no YAGNI case for the more restrictive option.

# Consequences

* Config format is a list from day one, even when today's actual usage has exactly
  one entry in it.
* If pairs ever need to interact (shared exclude-list caveats, ordering, concurrent
  access to the same clone) those are separate, concrete decisions to make if/when
  they come up — not implied by this one.
