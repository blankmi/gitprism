---
type: Decision
title: Destination anchors come from exact authenticated commit mappings, not global branch-topology inference
description: Supersedes decisions/0043's merge-base comparison across every mirrored sibling. Sync reconstructs an in-memory source-to-dest mapping index from the authenticated markers already stored in source and dest commits, then anchors each new or rewritten source branch at the nearest exactly mapped commit on that branch's first-parent history. Source-to-dest branches are processed by increasing distance from an existing mapping so a parent projection created earlier in the same non-interactive CI run becomes available to its children. No database, Git notes, dedicated mapping refs, developer command, or persistent CI cache is added; only incomparable canonical mappings for the same exact source commit halt for operator review.
tags: [architecture, branches, markers, ci]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-28T00:00:00Z }
---

# Context

[Decisions/0043](0043-mirror-only-branches-graft-onto-their-nearest-mirrored-ancestor.md)
fixed a real problem: a task branch forked from a mirror-only feature branch
must build on that feature branch's filtered dest history, not fall back to a
coarser setup graft that makes already-merged commits appear again in a pull
request. Its solution infers that parent by comparing the branch being created
or rebuilt with every other local source branch whose same-named dest ref
exists:

1. compute `merge_base(branch_tip, candidate_tip)` for every candidate;
2. compare all resulting merge bases by ancestry;
3. choose the unique most-specific group; and
4. resolve that group's source-side merge base into a dest-side commit by
   scanning each candidate's markers.

That is safe in the narrow sense that it refuses rather than guessing when the
merge bases are incomparable. It is not operationally robust enough for the
project's intended use: unattended CI where ordinary developers freely create,
merge, rebase, amend, and retarget their source branches without knowing that
gitprism exists. The set and topology of *other* branches become implicit input
to one branch's authorized mirror-only force rebuild. A stale sibling, a branch
that happens to share a merge base, or round-trip-generated history can change
the answer even though the rewritten branch's own first-parent line states a
clear base.

A real deployment produced exactly this failure after an ordinary workflow:

1. `develop` changed and round-tripped;
2. `develop` was merged into the round-tripped
   `feat/supplier-specific-accounting-data`;
3. mirror-only
   `tasks/supplier-specific-accounting-data/add_tax_tables` was rebased onto
   that feature branch; and
4. its normal rewrite rebuild halted because 0043 found two incomparable
   merge-base groups: `develop` at `5df74a1...`, and the feature branch plus
   another task branch at `da1c8b1...`.

The feature branch and the other task branch even shared the exact same merge
base, so 0043 correctly grouped them; the real ambiguity was between that group
and `develop`. That distinction does not make the operational result acceptable:
the developer had rebased the task onto the feature branch, yet adding or
removing other mirrored refs could determine whether CI accepted the rebuild.
Repeated operator reconciliation is contrary to the intended workflow. A real
content conflict or contradictory recorded state should require an operator;
an ordinary mirror-only branch rebase should not.

# Prior art

The prior art already recorded in `design/references/` points away from global
branch-topology inference:

* [josh](../references/josh.md) keeps persistent exact mappings between source
  and filtered commits and looks mappings up rather than rediscovering them from
  the current ref topology. Its persistence mechanism is a local database plus
  Git notes; gitprism does not need to adopt that storage mechanism to adopt the
  exact-mapping property.
* [Copybara](../references/copybara.md) records its origin revision in each
  generated commit and resumes from that recorded revision. It also exposes an
  explicit last-revision override instead of trying to infer intent from every
  branch in the repository.
* [git subtree](../references/git-subtree.md) records its mainline and split
  boundaries in marker trailers. Its marker is coarser than gitprism's per-commit
  markers, but it follows the same principle: resume from recorded mapping state,
  not from a comparison of unrelated refs.

Gitprism already has the durable data those tools need. An authenticated
`SourceToDest` marker on a dest commit maps the source counterpart named in the
marker to that dest commit. An authenticated `Setup` or `DestToSource` marker on
a source commit maps that source commit to the dest counterpart it names.
[Decisions/0003](0003-mapping-state-in-commit-trailers.md) chose commit-embedded
mapping state, and [decisions/0025](0025-authenticated-mapping-markers.md) made
those mappings safe to trust. The missing piece is an index over those existing
mappings for the duration of one run, not a new durable store.

# Decision

## Durable state remains in authenticated commits

No database, Git notes, dedicated `refs/gitprism/*` mapping refs, developer-side
metadata, or persistent CI cache is added. The authenticated markers already in
Git history remain the sole durable source of truth.

At the start of source-to-dest processing, after configured dest-to-source
branches have been reflected into source, `sync` reconstructs a bounded in-memory
mapping index from the fetched source and dest histories:

* a verified `SourceToDest` marker on dest commit `D`, naming source commit `S`,
  contributes `S -> D` with the marker's recorded branch as provenance;
* a verified `Setup` or `DestToSource` marker on source commit `S`, naming dest
  commit `D`, contributes `S -> D` with its recorded branch as provenance; and
* unverified, malformed, wrong-branch, or otherwise invalid markers contribute
  nothing, exactly as in every current resume and loop-prevention scan.

The index is a cache, not state. It may be discarded when the process exits and
must be reconstructible by a fresh CI clone from Git alone. Each source-to-dest
commit successfully pushed during the run is added immediately, so later
branches can build on mappings created earlier in the same invocation.

The existing static branch, marker-scan, and pending-history limits continue to
apply. Implementation adds an aggregate mapping-entry bound so reconstructing
the index cannot turn the per-scan bounds into unbounded branch-count multiplied
work.

## Resolve an anchor from the branch's own first-parent history

For a brand-new branch or a positively detected mirror-only rewrite — the two
call sites that currently invoke 0043's sibling search — walk that branch's
source history first-parent-only, newest to oldest. The first source commit with
an exact entry in the mapping index is the anchor boundary; its mapped dest
commit is the dest-space parent onto which the branch's remaining pending
commits are projected.

This is branch-local input: the ordered source commits the branch actually
descends from, plus authenticated mappings for those exact commits. Other branch
names are provenance for diagnostics and index construction, not candidates
whose pairwise merge bases compete to express the branch's intent.

First-parent is deliberate and consistent with
[decisions/0019](0019-marker-scans-are-first-parent-only.md) and
[decisions/0035](0035-pending-history-is-first-parent.md): gitprism already
defines a tracked branch's carried history by its first-parent line. A merge's
side history must not reappear here as a second, implicit parent choice after
those decisions excluded it from marker and pending-commit scans.

Commits that produced no dest commit because their filtered tree change was
empty simply have no `SourceToDest` mapping. The walk continues to the next
older exact mapping, preserving today's no-empty-commit rule.

## Process source branches by distance from known mapping state

Alphabetical source-to-dest processing is replaced with deterministic,
dependency-aware processing:

1. For every unprocessed source branch, walk first-parent until the nearest
   exact mapped ancestor and record the number of commits between it and the
   branch tip.
2. Process the branch with the smallest such distance first; break equal-distance
   ties by branch name for deterministic output.
3. Add every newly pushed mapping to the in-memory index.
4. Recompute the remaining branches' nearest mappings/distances and repeat.

This handles a parent and child both appearing before their first sync without a
configuration entry or developer action. The parent has fewer unmapped commits,
is projected first, and contributes the exact mapping the child then anchors on.
The Git commit DAG makes this dependency relation acyclic; there is no retry
guess or force escalation involved.

Configured dest-to-source processing remains first, as today. The mapping index
is built afterward so it sees source tips advanced by that phase. Progress-bar
totals remain based on the same branch names; only source-to-dest completion order
changes.

## Equivalent mappings are normalized; contradictory ones still halt

More than one raw dest OID for one source OID is not automatically a
contradiction. [Decisions/0044](0044-a-branchs-dest-ref-always-carries-its-own-marker.md)
deliberately creates content-empty, branch-scoped marker commits when a new
branch points directly at another branch's anchor. If source commit `S` maps to
feature commit `D1`, and task's self-accounting empty marker `D2` names the same
`S` on top of `D1`, both are ordinary records of the same reusable anchor.

The mapping index therefore canonicalizes exact mappings before resolving an
anchor:

1. A verified content-empty, single-parent `SourceToDest` marker — its tree is
   byte-identical to its parent's — is a branch-local self-accounting alias. For
   cross-branch anchoring it resolves to its parent, while the raw marker commit
   and branch remain recorded as provenance. This changes none of 0044's
   same-branch resume behavior.
2. Deduplicate identical resolved dest OIDs.
3. If multiple resolved dest OIDs remain but one is an ancestor of every other,
   choose that common ancestor. It is the least branch-specific exact projection
   and avoids importing later sibling-only ancestry. Record every discarded
   descendant as provenance for diagnostics.
4. Only two or more incomparable resolved dest OIDs are contradictory. Gitprism
   cannot know which published dest ancestry the new branch is intended to
   extend, so the branch halts with an error naming:

* the exact source OID;
* every incomparable canonical dest OID; and
* the marker branch provenance for each mapping.

Gitprism does not choose lexicographically, choose the newest commit, or compare
incomparable mappings by tree content and guess. Operator intervention remains
reserved for this genuinely underdetermined state and for real merge conflicts.

If no exact authenticated mapping exists anywhere on a branch's first-parent
history, the existing no-shared-history behavior remains: a discovered branch
warns and is skipped under decisions/0024; a configured branch's broken setup
invariant still fails loudly.

## Relationship to decisions/0043 and 0044

This decision supersedes decisions/0043's sibling enumeration, pairwise
`merge_base`, ancestry-domination, `Ambiguous`, and equal-merge-base resolution
algorithm. The user-visible instruction to merge or rebase until those inferred
merge bases become ordered is removed.

It does not supersede 0043's problem statement: task branches still need the
most specific already-projected ancestor so pull-request diffs do not repeat
their parent branch's commits. It also leaves 0043's later cross-branch
`DestToSource` loop-prevention amendment in force; those verified source-side
markers now contribute directly to the exact mapping index.

[Decisions/0044](0044-a-branchs-dest-ref-always-carries-its-own-marker.md)
also remains in force. When a branch ref is created or rebuilt directly at an
anchor without a content-changing commit of its own, its branch-scoped marker
commit still makes the new ref self-accounting on the next run.

# Why

* **Normal Git workflows become unattended again.** Rebasing or amending a
  mirror-only branch changes its own first-parent history. CI follows that new
  history to its nearest exact projected ancestor; developers do not run a
  gitprism command or reshape their history to satisfy a global heuristic.
* **The mapping already exists.** Source-to-dest commit generation records the
  exact source OID in every generated dest commit. Replacing that fact with a
  merge-base inference is less precise and creates failure modes the stored
  mapping was meant to avoid.
* **Unrelated branches cannot become competing anchors.** They may supply an
  exact mapping for a commit the branch really descends from, but their current
  tips and pairwise merge bases are no longer input.
* **Fresh CI clones remain sufficient.** The index is reconstructed from fetched
  authenticated commits on every run. There is no runner-local state to preserve,
  restore, lock, or recover.
* **Operator intervention has a narrow meaning.** Empty branch aliases and a
  single comparable mapping chain normalize automatically. Only incomparable
  canonical dest histories for one exact nearest source ancestor are genuinely
  contradictory; stopping there follows the project's safety rule without
  turning ordinary rebases into operator work.

# Consequences

* The source-to-dest phase is no longer alphabetical. It is deterministic by
  nearest-mapping distance, then branch name.
* `RunCache` grows into a per-run mapping/index cache and is populated both from
  initial authenticated scans and from successful pushes.
* The O(branches squared) remote-existence/merge-base candidate search in
  `dest_anchor_for_branch` is removed. Mapping reconstruction may still inspect
  every bounded branch history, but each verified mapping is indexed once and
  reused by every branch in the run.
* Existing marker format and `GITPRISM_STATE_KEY` remain unchanged. No migration
  or setup rerun is required; existing authenticated history supplies the index.
* Mirror-only rebuilds keep decisions/0039's positive rewrite detection and
  decisions/0040's force-with-lease compare-and-swap. This decision changes the
  rebuild anchor, not who may force or how concurrent dest movement is protected.
* Round-tripped branches remain fast-forward-only in both directions under
  decisions/0038.

# Implementation plan

1. **Capture the production failure first.** Add an end-to-end test with a
   round-tripped `develop`, a round-tripped feature containing its change, a
   sibling task, and a mirror-only task rebased onto the feature. Confirm the
   current implementation halts with 0043's ambiguous merge-base groups.
2. **Introduce the bounded mapping index.** Add focused tests for authenticated
   `SourceToDest`, `Setup`, and `DestToSource` entries; identical duplicate
   mappings; decision 0044's empty-marker aliases; comparable and incomparable
   mappings; invalid MACs; marker branch provenance; and the aggregate entry
   limit. Then implement index reconstruction in
   `sync/marker_scan.rs` or a dedicated `sync/mapping_index.rs` module.
3. **Resolve anchors first-parent-only.** Add tests proving the nearest exact
   mapping wins, a mapped merge side-parent is ignored, filtered-empty commits
   are walked past, empty aliases normalize to their parents, comparable exact
   mappings choose their common ancestor, and only incomparable canonical
   mappings halt with precise diagnostics. Replace 0043's global sibling
   merge-base selection in
   `sync/anchor.rs`.
4. **Schedule by mapping distance.** Add a test where a new mirror-only feature
   and its new task child both appear before either has a dest ref. Confirm the
   parent is projected first regardless of lexical branch names and the child
   uses the mapping created in the same run. Then replace the alphabetical
   source-to-dest loop with deterministic distance ordering.
5. **Preserve established safety behavior.** Re-run and, where necessary,
   adapt the rewrite matrix: rebase, amend, reset-plus-new-commit, stale clone,
   concurrent dest movement, round-tripped refusal, branch-scoped empty marker,
   cross-branch `DestToSource` loop prevention, policy mismatch, and real
   conflict resolution. No test may weaken fast-forward-only round-tripped
   pushes or mirror-only force-with-lease.
6. **Remove superseded machinery and wording.** Delete the pairwise sibling
   merge-base groups, `DestAnchor::Ambiguous`, their error helper, and tests that
   assert the superseded behavior. Replace the old ambiguity error with the new
   incomparable-canonical-mapping diagnostic. Update README behavior where
   branch anchoring is described.
7. **Verify the complete change.** Run formatting, the full locked test suite,
   Clippy with warnings denied, and the locked release build. Record exact test
   counts and results in `design/log.md` when implementation lands.
