# Plan PERF-003 — memoize lookups that a tainted index cannot change

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, PERF-003 |
| Severity / priority | LOW / P2 |
| Effort | Small once CODE-007 exists |
| Decision required | No — extends 0046 Addendum 2 Finding I's memo with the same soundness argument; note in `log.md` |
| Depends on | CODE-007 |
| Status | Proposed |

## Problem

`mapping_distance_with_none_memo` (`src/commands/sync/mod.rs:130-143`) memoizes
only a `None` distance. When the index is tainted (`is_truncated()`), every
branch's `nearest_first_parent_mapping_with_distance` returns `Contradictory`
either at its first mapping or after walking to the horizon (`mapping_index.rs:399-404`).
That result is stable for the run, but the scheduling loop at `:318-324`
recomputes it every round: Θ(B²) walks, each up to `MAX_MARKER_SCAN_COMMITS`.

## Soundness

`truncated_scans` is written only by reconstruction and
`note_incomplete_reconstruction`, both before scheduling starts. A tainted
lookup's outcome is therefore fixed for the run. A branch's *own* horizon
(`OwnScanHorizon`) is not: a push earlier in the run can place a mapping inside
it. Only causes with `is_stable_for_run()` may be memoized.

## Steps

### Step 1 — failing test

**Files.** `src/commands/sync/tests/scheduling.rs`.

**Test first.** Reconstruct an index with `reconstruct_with_scan_limit` small
enough to taint it; three unmapped branches; drive
`select_next_branch_by_mapping_distance` through three rounds with a counting
`distance_for`. Assert each branch's walk ran once, not three times. Second
test: an untainted index where a branch's own walk hits `OwnScanHorizon`, then a
mapping is recorded within the horizon; the next round must recompute and find
it.

### Step 2 — widen the memo

**Files.** `src/commands/sync/mod.rs:130-143`, `mapping_distance_for_branch`.

**Change.** `mapping_distance_for_branch` returns the `MappingLookup` alongside
the distance (or a small struct). The memo stores the full result when the
distance is `None` or when the lookup is `Contradictory(cause)` with
`cause.is_stable_for_run()`. Rename to `mapping_distance_with_stable_memo`; the
doc comment cites Finding I and this plan.

### Step 3 — record

`design/log.md` entry; one sentence appended to 0046 Addendum 2 Finding I
("extended to stable contradictions, 2026-09-xx").

## Verification

`cargo test scheduling`; the count-based assertions; fmt and clippy clean.
