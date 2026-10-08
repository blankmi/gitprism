---
type: Decision
title: First-parent scans follow parent links directly; the complete-scan horizon is a stated capacity limit
description: Every first-parent marker or mapping scan replaces its `Revwalk` (TOPOLOGICAL + simplify_first_parent) with a plain `parent_id(0)` loop. libgit2 pre-walks and sorts the entire reachable history before a sorted revwalk returns its first commit, so each scan cost the whole history regardless of where it stopped; with decisions/0046's per-round distance scheduling that made a no-op sync of 46 branches over 11.5k commits take 51 s instead of ~5 s. Same order, same limits, same results. Also replaces the misleading "rewritten outside gitprism?" context on `pending_dest_commits` errors, and records the ~100k first-parent-commit / 100k mapping-entry ceiling that decisions/0046 and 0048's complete scans impose under decisions/0032.
tags: [architecture, performance, limits]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-10-08T00:00:00Z }
---

# Context

Review of v0.1.7 → `50bdcc3` before a production upgrade (2026-10-08), on a
synthetic pair, local file remotes, no-op `sync`:

| History | Branches | v0.1.7 | `50bdcc3` |
| --- | --- | --- | --- |
| 20k commits | 1 | 0.2 s | 0.5 s |
| 99k commits | 1 | 0.5 s | 2.1 s |
| 11.5k commits | 46 | 3.8–6.5 s | 51–52 s |
| 99k commits | 31 | 3.9 s | 205 s |

A profile puts ~90% of the time in
`select_next_branch_by_mapping_distance` →
`MappingIndex::nearest_first_parent_mapping_with_distance`, inside
`git_revwalk_next`. In libgit2 1.9 (`libgit2-sys` 0.18.7,
`src/libgit2/revwalk.c`), any sorting other than `GIT_SORT_NONE` sets
`walk->limited`, and the first `git_revwalk_next` calls `prepare_walk` →
`limit_list`, which loads every reachable first-parent commit before
returning the first one. A distance walk whose nearest mapping is one commit
away therefore still parses the whole history, and scheduling repeats it for
every remaining branch every round.

The same `Revwalk` shape is used by `mapping_index::first_parent_walk`
(reconstruction and distance lookup) and by `marker_scan`'s
`scan_for_dest_marker`, `newest_source_marker` and
`dest_to_source_marker_targets`. Each scan's
`MAX_MARKER_SCAN_COMMITS` check counts yielded commits, so the pre-walk
itself was never bounded by decisions/0032.

Separately, at 100.5k first-parent commits one customer commit on a
round-tripped branch made `50bdcc3` fail the whole run with
`dest-to-source boundary's case-2 source scan exceeds the 100000 commit limit`,
wrapped in "has dest branch … been rewritten outside gitprism?". v0.1.7
imported the same commit.

# Decision

**1. First-parent scans follow `parent_id(0)`.** One helper yields
`tip`, then each commit's first parent, until a root. It replaces the
`Revwalk` in `mapping_index::first_parent_walk` and in `marker_scan`'s three
scans. For a single first-parent chain this is the exact sequence the sorted
revwalk produced: a chain has one child→parent order, independent of commit
timestamps and of merge side parents. Each caller keeps its own limit check
and visited-set early exit *before* loading the next commit, so neither the
limit nor the shared visited set (decisions/0046 addendum, Finding A) can be
bypassed by an unbounded pre-walk. An object error for a commit the scan
actually reaches is still an error; only commits beyond where a scan stops are
no longer read.

`TOPOLOGICAL | REVERSE` walks (`pending_commits`, `resolve`) are unchanged:
they need the full oldest-first list anyway.

**2. `pending_dest_commits` callers use a neutral context.** The three
callers (`sync` dest→source and both `resolve` dest→source paths) wrap errors
with "finding dest commits pending for branch …" instead of "has dest branch
…'s history been rewritten outside gitprism?". The underlying errors already
say what failed (B1 not an ancestor, B1 not on the first-parent line, a scan
limit), and a limit error is not a rewrite.

**3. The complete-scan ceiling is stated, not changed.** Decisions/0046's
index and decisions/0048's case-2 target set both need complete scans. Under
decisions/0032 a pair is fully served only while every scanned first-parent
chain is ≤ `MAX_MARKER_SCAN_COMMITS` (100,000) and the reconstructed index
stays ≤ `MAX_MAPPING_ENTRIES` (100,000) across all heads. Beyond that:

* An incomplete decisions/0046 reconstruction (either limit) halts anchoring:
  a new branch's first mirror and a mirror-only rewrite rebuild.
* A decisions/0048 case-2 source-history scan over 100,000 commits fails the
  run when a round-tripped branch has a dest-native commit to import. The
  mapping-entry limit does not affect case 2.

Existing branches continue, subject to their own resume-point and
pending-history limits; a dest→source run with nothing to import never
reaches case 2. The limits are not raised: a larger constant
only postpones the same ceiling. A scan that could stop before the root needs
a proven boundary or a checkpoint (stopping at the newest marker loses older
exact mappings; stopping at `Setup` needs an invariant v1 markers don't carry),
which is an open question in requirements/0001.

# Why

* **The cost was the walk primitive, not the design.** Decisions/0046's
  anchor and distance lookups stop at the nearest mapping; the sorted revwalk
  made every one of them a full-history scan. Reconstruction is a complete
  scan by design and stays one. With direct parent links a scan
  costs what it inspects.
* **The primitive is already in use.** `marker_scan::dest_to_source_boundary`
  and `anchor::b1_reachable_via_first_parent` already walk `parent_id(0)` with
  a limit check per step.
* **No new behavior to justify.** Same sequence, same limits, same errors for
  reached commits. Per AGENTS.md, nothing here is new automation.

# Consequences

* Measured with the change applied to `mapping_index::first_parent_walk`
  only: 11.5k commits, 46 branches, no-op sync 51 s → 4.8–6.1 s.
* Scheduling still recomputes every remaining branch's distance every round
  (B²/2 walks); each walk now costs its distance to the nearest mapping. A
  branch with no mapping near its tip still walks far. Decisions/0046's
  scheduling is unchanged.
* Reconstruction still reads every head's full first-parent history once per
  side per run (shared visited sets); that cost is linear in history, not in
  branches × history.
* A clone missing a commit below the point where a scan stops no longer fails
  that scan. A missing commit the scan needs still fails, and complete scans
  (reconstruction, case 2) still need all ancestry up to their limit.
* The decisions/0049 bulk-fetch race (a dest branch deleted between listing
  and fetch fails the run once) is unchanged; 0049 already accepts it.

# Rejected alternatives

* **Memoize scheduling distances once per run.** An accepted push can shorten
  a later branch's distance, so a per-run memo is unsound. General
  mapping-aware distance caching is deferred; it is not needed for the
  measured cost. PERF-003's narrower memo of stable tainted-index outcomes
  stays as planned.
* **Bound case 2 at the newest branch-scoped import.** Decisions/0048 requires
  `DestToSource` markers of any branch; an older foreign import can represent a
  dest-native commit above B1, so this would reintroduce phantom replays.
* **Raise `MAX_MARKER_SCAN_COMMITS`.** Postpones the ceiling and widens
  decisions/0032's bound without a reason specific to the data.
* **Classify limit errors by message text.** Fragile; the neutral context plus
  the originating error's own message is enough.
