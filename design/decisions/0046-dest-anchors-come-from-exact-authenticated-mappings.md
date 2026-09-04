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
* unverified, malformed, or otherwise invalid markers contribute nothing.
  Marker HMAC verification authenticates the branch recorded inside the
  marker, so a valid marker inherited while scanning another branch is still
  accepted; the scanning head's name is not a second branch-scope check.

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

# Addendum (2026-08-31): reconstruction is deduplicated and horizon-bounded, not full-history-and-fail

A repository review found that `MappingIndex::reconstruct`'s own scans
(`scan_source_history`/`scan_dest_history`) walked every scanned head's
*complete* first-parent history with no early exit, and that both
`MAX_MARKER_SCAN_COMMITS` and `MAX_MAPPING_ENTRIES` were hard `anyhow::bail!`
limits inside `reconstruct_mapping_index`, called unconditionally from `run()`
with `?`. A repository whose scanned history genuinely crossed either bound —
100,000 first-parent commits on some branch, or 100,000 lifetime mapping
entries, both realistic for a long-lived deployment since every
gitprism-generated dest commit is one permanent, never-pruned entry — failed
*every* future run before anything synced, with no operator remedy short of
history surgery. That contradicts this decision's own "never normally hit
fail-safe" framing of these limits (decisions/0032) and the project's rule
that a limit with no operator remedy is a design bug, not safety. Separately,
every scanned commit was parsed twice (once by a caller's own `marker::parse`
pre-check, once again inside `marker::verify`), with the pre-parse running
before `verify`'s own size gate could apply to it.

## Decision

**Parse once, gated once (Finding D).** `marker::verify_self` is a new
sibling of `marker::verify` for exactly the self-verification shape
`add_source_commit`/`add_dest_commit`/`record_pushed_dest_commit` already
used (verifying a marker against its own recorded branch, never against the
scanning head's name): both now share one internal `verify_parsed`
implementation that size-gates and parses the message exactly once and
returns the full `ParsedMarker` on success, instead of a caller pre-parsing
the message and `verify` parsing it again internally. `verify`'s own
behavior and signature are unchanged.

**Shared visited set removes the multiplied work (Finding A, part 1).**
`MappingIndex::reconstruct` now threads one `HashSet<Oid>` through every
`scan_source_history` call and a separate one through every
`scan_dest_history` call (kept per-side, not combined, because a source
commit and a dest commit can legitimately share an OID at the `setup` graft
while requiring different marker directions to be checked). A commit already
visited by an earlier head's walk this reconstruction is not re-loaded or
re-parsed; that walk simply stops there, since first-parent history is
deterministic and everything from that shared commit onward was already
either recorded or already marked truncated by whichever walk reached it
first. Total reconstruction work is now O(unique first-parent commits
scanned), not O(heads × history) — this is what makes this decision's
original "each verified mapping is indexed once and reused by every branch"
consequence actually hold under repeated/overlapping branch histories, not
just under a single linear one.

**Both bounds are now a horizon, not a run-ending failure (Finding A part 2,
Finding B).** `MAX_MARKER_SCAN_COMMITS` and `MAX_MAPPING_ENTRIES` are both
still enforced, but hitting either during one head's reconstruction scan now
keeps every mapping already found, appends a human-readable note to the
index's own `truncated_scans`, and stops that one walk — never an
`anyhow::bail!`. `record_bounded` (used only by reconstruction) returns
whether it actually recorded, rather than erroring, so the calling scan can
treat "no more room" exactly like "no more scan budget." A mapping this run's
own push already produced — `record_built_mapping` and the new
`record_pushed_dest_commit` (replacing `add_dest_commit` at the
accepted-push call site in `run()`) — go through a separate, always-succeeds
`record_unbounded` instead: that mapping is already bounded by the
pending-history limits before dest was ever mutated, so recording it in the
index must never fail an otherwise-successful run. `MAX_MAPPING_ENTRIES` now
means "how much of reconstruction's own scan work to do," the same kind of
bound `MAX_MARKER_SCAN_COMMITS` already was, rather than a cap on how much
authenticated history may exist (which nothing enforces, and which is why
exceeding it recurs every run — Addendum 2, Finding P). The entries retained before this horizon are not
necessarily the newest or oldest globally: scan order is determined by the
source/dest head order and shared-history traversal.

**A truncated horizon never silently downgrades an outcome.** The one
`MappingIndex` a run builds tracks whether any scan was truncated at all
(`is_truncated`). `nearest_first_parent_mapping_with_distance` — the single
walk both anchor resolution and distance scheduling call — treats its own
scan exceeding `MAX_MARKER_SCAN_COMMITS` as a `Contradictory` result naming
the limit, not an `anyhow::bail!`. It also refuses *any* exact lookup while
the reconstructed index is incomplete, including a source commit that already
has a recorded mapping: an unscanned marker may provide an incomparable
mapping for that same source commit, and resolving the visible mapping would
silently hide the contradiction. A
`Contradictory` result already gets decisions/0045's per-branch-halt
treatment everywhere it's produced (`dest_anchor_for_branch` →
`Outcome::Error`, `return Ok(true)`; `mapping_distance_for_branch` still
returns a usable distance so scheduling isn't starved either) — no new halt
plumbing was needed, only reusing the existing shape for a new cause.

This conservative choice has an unavoidable availability tradeoff under the
no-persistent-state constraint. After a bounded scan, the observed index is
identical whether an omitted commit contains no marker or contains an
incomparable mapping for a source commit already observed. A resolver that
returns an anchor in both worlds would violate the contradiction-safety
invariant; a resolver that scans the omitted history to distinguish them has
abandoned the bound. Therefore a repository whose history remains beyond the
horizon can repeatedly halt mapped branches until an operator changes the
history or a future decision changes the static implementation limit. Avoiding
that repeated halt requires a durable scan cursor/summary or an unbounded
verification pass, both outside this decision. The horizon still prevents a
whole-run error, and branches with a complete index are unaffected — but
because every dest commit gitprism generates adds one authenticated mapping to
history permanently, a repository that grows past the horizon truncates on
every subsequent run, and from then on every new branch's first mirror and
every detected rewrite's rebuild halts until an operator acts (see Addendum 2,
Finding P). "Not a lifetime ceiling" describes what the constant bounds —
reconstruction work per run — not the resulting operational availability.

An exact mapping whose canonical destination object is absent from the local
object database is also a per-branch refusal, even when an older mapping is
available. Walking past it can skip destination-native content represented by
a `DestToSource` marker, which replay intentionally loop-prevents; the
operator must fetch the missing object or resync from a complete clone.

**Scheduling computes each round's distances once (Finding C).** The
distance-based scheduling loop in `run()` previously called
`mapping_distance_for_branch` for both sides of every pairwise comparison,
recomputing the already-selected branch's distance on every iteration
(~2·B² revwalks across a whole run). `select_next_branch_by_mapping_distance`
is a new, pure per-round helper: it computes each remaining branch's
`(distance, name)` key exactly once, then takes the minimum. Distances are
still recomputed at the start of every round (not cached across rounds),
since a push earlier in the same run can add a mapping a later round's
distance depends on — this decision's own scheduling rationale. A single
branch's distance computation failing (most commonly Finding A's own-scan-
horizon refusal now, but any error) is routed to that branch's own
decisions/0045-shaped per-branch halt instead of aborting the round or the
run; the round is retried with that branch removed.

## Why

The original bounds were justified as protection against *per-run* multiplied
work, never as a lifetime ceiling on authenticated history. The visited set
removes the branch-count multiplier, and the aggregate bound remains a
reconstruction work horizon rather than a lifetime entry ceiling. A bounded
scan nevertheless cannot prove that an omitted marker does not contradict a
visible one. Degrading that uncertainty to a per-branch refusal preserves
the project's "stop and involve the operator" safety rule and never fails
after dest has already been mutated by an accepted push. Under the current
no-persistent-state constraint, repeated refusal while a repository remains
beyond the horizon is the explicit availability tradeoff documented above.

## Consequences

* `MappingIndex` gains `scan_limit` (mirroring the existing `limit` field for
  aggregate entries) and `truncated_scans`; both are test-overridable
  (`with_scan_limit`, `with_limit`, `reconstruct_with_scan_limit`) so
  truncation is tested without needing a real 100,000-commit repository.
* `add_source_commit` and `add_dest_commit` now return `Result<bool>` (was
  `Result<()>`) — `false` means the aggregate bound was hit, signaling the
  calling scan to stop. Both are `pub(crate)` but have no callers outside
  `mapping_index.rs`'s own scans.
* `run()`'s accepted-push arm calls the new `record_pushed_dest_commit`
  instead of `add_dest_commit`, so an index bookkeeping limit can never fail
  a run after dest has already been mutated.
* ForceMirrorOnly invalidation plans one bounded ancestry walk before the
  force-with-lease push and applies only the resulting infallible cache
  deletion after acceptance; missing old destination objects are stale
  mappings, not post-push errors.
* `marker::verify`'s signature and behavior are unchanged; `marker::verify_self`
  is new and used only by `mapping_index.rs`'s reconstruction scans.
* **Tests added:** shared first-parent history across two branches is
  scanned once, not once per branch (a call-count assertion, since a real
  100,000-commit repository is impractical in a unit test); a scan exceeding
  its commit horizon keeps prior mappings and returns `Ok`, not `Err`; a
  branch whose own walk exceeds the horizon with no mapping found refuses
  per-branch; an unrelated branch's own complete, mapping-free walk still
  refuses once any scan this run was truncated; a visible mapping beyond an
  incomplete horizon cannot mask a contradiction; exact mappings with
  missing canonical destinations halt rather than walking past them
  (including an end-to-end rewrite that would otherwise loop-prevent and lose
  imported content); a reconstruction scan hitting the aggregate entry limit
  stops without erroring; `record_built_mapping` never fails past the
  aggregate limit; `verify_self` rejects an oversized message before parsing
  it; and `select_next_branch_by_mapping_distance` computes each branch's
  distance exactly once per round, breaks ties by name, and turns a single
  branch's distance error into just that branch's halt.

# Addendum 2 (2026-08-31): every new bound is a horizon or a per-branch halt, never a whole-run abort

## Context

A second review of this decision's implementation branch confirmed ten
findings, all in code this decision added. Four are availability regressions
against this decision's own Addendum 1 direction: the implementation
introduced three fresh whole-run abort sites and left one post-push
bookkeeping call fallible, while Addendum 1's stated rules are "a limit with
no operator remedy is a design bug, not safety" and "never fail a run after
dest has been mutated." Four are cost or dead-code findings where a doc
comment claims an optimization the code does not implement. Two are
bookkeeping: decisions/0043 carries none of this repository's supersession
signposting, and one test's name still describes the bug it now proves fixed.

This addendum records the resolutions. It changes behavior, so it is written
before the code, per AGENTS.md.

## Decision

**Finding E — force-rebuild invalidation no longer walks the whole
replacement DAG, and has no bound to hit.** Addendum 1 replaced
`plan_replaced_chain_invalidation`'s per-mapping ancestry queries with a
single reachability walk of `new_dest_tip`. That walk is the only scan in
`mapping_index.rs` without `simplify_first_parent`, yet it spends
`MAX_MARKER_SCAN_COMMITS` — a budget every other call site calibrates against
first-parent history — and `anyhow::bail!`s on exceeding it. Because
`sync_pair_to_dest_with_key`'s error propagates through `run()`'s `?`, a dest
whose tip reaches more than 100,000 commits makes every mirror-only rewrite
kill the whole run and cost every not-yet-processed branch its turn, with no
operator remedy short of shortening dest's history.

That walk is reverted to per-destination reachability queries, which is what
Git already provides a safe primitive for (`git_graph_descendant_of`), with
the two properties Addendum 1 wanted from the walk kept explicitly:

* Only records provenanced to `branch` are considered, and their
  `canonical_dest` OIDs are deduplicated into one memo per plan, so each
  distinct destination is queried at most once regardless of how many source
  commits map to it.
* A destination absent from the local object database is stale by
  definition — established with `Repository::find_commit` before any
  reachability query, exactly as `resolve_for_anchor` already does — rather
  than by relying on a reachability query not being asked about a missing
  OID.

`new_dest_tip` itself counts as reachable (a mapping to the replacement tip
is not stale). The function keeps its `Result` for genuine object-database
failures, but no longer contains a limit an operator cannot act on. It is
still planned before the push and applied infallibly after acceptance.

**Finding F — the destination branch listing is a horizon, not an abort.**
`git::remote_branch_names` applies `MAX_SOURCE_BRANCHES` to dest's branches
and bails, and `reconstruct_mapping_index` is called unconditionally with
`?`, so a destination repository that independently accumulates more than
4,096 branches fails every sync before any branch is processed. gitprism has
never constrained dest this way, and dest's branch count is not something a
source-side operator can necessarily reduce.

`remote_branch_names` instead returns the names it read up to the limit
together with an explicit "this listing was truncated" signal, and
reconstruction records that truncation into the same `truncated_scans` an
over-horizon history scan records (via a new
`MappingIndex::note_incomplete_reconstruction`). The reasoning is identical
to Addendum 1's: an unlisted dest branch may carry an incomparable mapping
for a source commit already observed, so exact lookups against that index
must refuse per-branch rather than resolve, and must not silently resolve the
visible mapping. What changes is that this is now the same conservative
per-branch refusal every other incompleteness produces, not a run that never
starts.

**Finding G — the list/fetch race is recovered for a deleted ref and
propagated for anything else.** `reconstruct_mapping_index` seeds
`dest_ref_exists` to `true` for every name `ls-remote` advertised, then
fetches each one individually. A ref deleted in that window makes the fetch
fail, and the failure aborts the whole run — previously a dest ref problem
was scoped to its own branch.

The two failure classes are resolved differently, and deliberately so:

* *The ref no longer exists.* Recoverable, because the recovered state is
  complete: the branch genuinely has no dest ref, so it contributes no
  mappings. On any fetch failure during reconstruction, the dest listing is
  refreshed once per run; if the branch is absent from the refreshed
  listing, it is recorded as `dest_ref_exists = false`, dropped from the
  reconstruction set, and the run continues.
* *Any other fetch failure* (authentication, network, transport, a corrupt
  remote). **Not** recoverable and explicitly not demoted to a per-branch
  halt: skipping the branch would leave the global mapping index incomplete
  while every other branch's lookup still believed it complete — the exact
  "a visible mapping hides an unscanned contradiction" hole Addendum 1
  closed. If the branch is still advertised by the refreshed listing, the
  original fetch error propagates as a run failure, before dest is mutated.

A per-branch halt is the right shape for a per-branch fact. Reconstruction's
completeness is a whole-run fact, and it stays one.

**[Amended by decisions/0049: this per-branch fetch and its list/fetch race
recovery are removed — the bulk namespace fetch is all-or-nothing, so a ref
deleted in that window now fails the whole fetch before any mutation.]**

**Finding H — the post-push bookkeeping path is genuinely infallible.**
Addendum 1 states that recording a mapping this run's own push authored
"always succeeds," and restructured `run()`'s accepted-push arm specifically
to guarantee it. `record_pushed_dest_commit` remained fallible through
`dest_commit_mapping`'s `repo.find_commit(oid)?`, called with `?` after the
push landed. Both `record_pushed_dest_commit` and `record_built_mapping` now
return `()`. A commit this call just authored that cannot be re-read records
no mapping rather than failing the run: the consequence of a missing entry is
a later branch finding no anchor and halting per-branch, never anchoring
wrongly. `record_built_mapping`'s `#[allow(clippy::unnecessary_wraps)]`
rationale — "leaves room for a future genuinely-fallible check" — is
withdrawn: the claim in the decision text is the stronger commitment, and a
future fallible check would be a decision, not a signature convenience.

**Finding I — a branch with no mapping is asked once per run, not once per
round.** Addendum 1's Finding C removed the pairwise recomputation but not
the per-round one, so scheduling remains Θ(B²) first-parent walks across a
run, and the walk for a branch with *no* mapping is the expensive case: it
traverses that branch's entire first-parent history up to
`MAX_MARKER_SCAN_COMMITS` before returning `None`.

Those results are cached for the rest of the run. The invariant that makes
this sound: a branch that could gain a mapping from another branch's
successful push already has that push's prerequisite anchor in its own
ancestry, and would therefore already have reported a distance. A `None`
distance can never become `Some` mid-run. Branches that *do* have a mapping
are still recomputed every round — required, since a push earlier in the run
can shorten their distance — and their walks stop at the first mapping found.

**Finding J — non-existence is cached too.** `reconstruct_mapping_index`
seeds `dest_ref_exists` only with `true`, so every source branch with no
same-named dest ref still spends its own `ls-remote` subprocess later, and
the comment claiming per-branch remote queries were eliminated is false. The
complement is now seeded `false` from the same listing, which is also what
Finding G's refresh path updates.

**Finding K — the dead per-run destination tip cache is removed.**
`RunCache::dest_tip`'s only reader is `fetch_dest_tip_cached`, whose only
caller is `reconstruct_mapping_index`; both writes in `mod.rs` happen after
reconstruction has finished and are never read. The cache and its helper are
deleted and reconstruction fetches directly, since the branch names it
iterates are already deduplicated. `sync_pair_to_dest_with_key`'s own
per-branch fetch is deliberately *kept*: decisions/0040 requires a
`ForceMirrorOnly` lease to be built from the dest tip as actually fetched by
that attempt, and the race-retry loop must refetch after a
`RejectedRefMoved` rejection. The duplicate fetch is the price of lease
freshness, stated here rather than left looking like an oversight.

**[Amended by decisions/0049: "the duplicate fetch" now names exactly one
duplicate, the source→dest lease fetch — reconstruction's own fetch is
replaced by the shared bulk-fetch namespace.]**

**Finding L — `loop_prevented` parses once.** It calls
`marker::verify(commit, branch, &[Setup, DestToSource], ...)`, then on
failure re-reads the message, re-parses it, and calls `verify` again with the
marker's own branch. Since `verify`'s branch check exempts `Setup` outright,
that union is exactly `marker::verify_self(commit, &[Setup, DestToSource],
None, key).is_some()` — the single call this addendum's own `verify_self`
was introduced for. Behavior is unchanged and covered by the existing
cross-branch loop-prevention regression test.

**Finding M — the test wrapper propagates reconstruction errors.**
`tests::sync_pair_to_dest` rebuilds `run_cache.mapping_index` with `?`, so a
reconstruction regression is catchable through the wrapper exactly as
through `run()` itself — no test can pass against a stale index left by an
earlier call on some other branch. The accommodation this replaced (reset
the index to empty on error) existed because `tests::fresh_clone_of_branch`
reused a `Repository` handle opened before its own external `git fetch
--depth=1` subprocess wrote `.git/shallow`, so a first-parent walk against
that stale handle raised a spurious `git2::Error { code: NotFound, class:
Odb }` — a fixture artifact, not a real limitation of reconstruction
(confirmed by direct investigation, design/log.md 2026-08-31). The fixture
now reopens the repository after the external fetch, before any use of the
handle, matching what a real gitprism process already does at every
`Repository::discover` startup; all three of its callers behave identically
under the reopened handle. A genuine object-database failure — an unreadable
object, `class=Os, code=Locked` — still propagates through reconstruction by
design: neither `mapping_index.rs` nor `anchor.rs` swallows or bounds it.

**Finding N — decisions/0043 gets this repository's supersession
signposting.** 0043 keeps `status: stable` with no inline supersession notes,
and its `decisions/index.md` entry does not say it is superseded, against the
convention decisions/0036 and decisions/0038 already establish. Since
AGENTS.md makes `design/` the source of truth, a reader landing on 0043 today
is handed a deleted algorithm as current. Its status, a header note, inline
notes on the clauses this decision replaced, and its index entry are updated.
0043's *problem statement* is not superseded — it is this decision's Context.

**Finding O — one test name is corrected.**
`run_reproduces_the_round_tripped_feature_rebase_anchor_ambiguity` now
asserts the fixed anchoring behavior, not the ambiguity it was written to
reproduce. Renamed to
`run_anchors_a_round_tripped_feature_rebase_on_its_exact_mapping_instead_of_halting`.

**Finding P — Addendum 1's "not a lifetime entry ceiling" is narrowed.**
That sentence is accurate about what the constant *means* (how much
reconstruction work one run does) but reads as contradicting the
availability paragraph directly above it. Every dest commit gitprism
generates is one more authenticated mapping in source and dest history
forever, so a repository that grows past the horizon truncates on *every*
subsequent run, and from then on every new branch's first mirror and every
detected rewrite's rebuild halts until an operator acts. The tradeoff is
accepted, not reopened here; the wording is corrected to state the
operational consequence rather than only the constant's scope.

## Why

Three of these four availability findings share one cause: a bound was added
for a real safety reason and then wired to `anyhow::bail!` because that was
the shortest path, in a module whose own decision text had already settled
that bounds are horizons. The distinction that decides the shape is not how
serious the condition is but *whose* fact it is. A branch's own history, its
own dest ref, its own rewrite — per-branch halt. Reconstruction's
completeness, which every branch's lookup depends on — whole-run, and the
index's existing truncation refusal already expresses it correctly. Finding
G is the case where those two rules meet, and the answer follows the fact,
not the convenience: a deleted ref is per-branch and recoverable, an
unreachable remote is not.

Finding E is also a case of Git already having the primitive. Addendum 1
moved away from `graph_descendant_of` to avoid asking it about a missing
object; existence-gating first solves that directly, and keeps the bound out
of the design.

## Consequences

* `plan_replaced_chain_invalidation` no longer references
  `MAX_MARKER_SCAN_COMMITS`; the only limit reachable from a force-rebuild
  path is the aggregate mapping-entry horizon, which cannot abort a run.
* `git::remote_branch_names` returns the listing plus whether it was
  truncated (its callers must handle both); `MappingIndex` gains
  `note_incomplete_reconstruction`, which makes an externally observed
  incompleteness taint lookups the same way a truncated internal scan does.
* `MappingIndex::record_built_mapping` and
  `MappingIndex::record_pushed_dest_commit` return `()`; `run()`'s
  accepted-push arm has no `?` after the push succeeded.
* `RunCache` loses `dest_tip`; `anchor::fetch_dest_tip_cached` is removed.
  `RunCache::dest_ref_exists` is now seeded for every source branch and every
  advertised dest branch, and is corrected in place when a listed ref turns
  out to be deleted.
* A destination with more than `MAX_SOURCE_BRANCHES` branches now syncs
  already-mirrored branches normally and halts only new branches and detected
  rewrites, instead of failing every run outright.
* **Tests added:** a force-rebuild whose replacement history exceeds the old
  invalidation limit still pushes and still invalidates its orphaned
  mappings; a truncated dest branch listing taints exact lookups instead of
  failing reconstruction; a dest branch deleted between listing and fetch is
  recovered by refreshing the listing, while a fetch failure whose ref is
  still advertised propagates; a source branch with no dest ref costs no
  per-branch remote query; a branch with no mapping has its distance computed
  once per run rather than once per round; `loop_prevented` behavior is
  unchanged after collapsing to `verify_self`; and the test wrapper
  propagates a reconstruction failure rather than reusing a previous
  branch's index.

# Addendum 3 (2026-09-04): three implementation rules recorded

## Context

A repository review found three behaviours of this decision's implementation
cited in code only by a three-letter shorthand of the form "F", hyphen,
a letter (A, B, or C), with no matching entry anywhere in `design/`. This
decision's own addenda already number their findings A–P, so that shorthand
reads as "Finding A" and so on — different, unrelated rules (e.g. Addendum
1's parse-once change is its actual Finding D). AGENTS.md makes
`design/decisions/` the source of truth; these three behaviours existed only
in comments. This addendum gives them their own names, continuing the
addenda's letter sequence, and the citing code comments are updated
(`decisions/0046 Addendum 3, Finding Q` / `Finding R` / `Finding S`) to match.
No behaviour changes; this is documentation of what the implementation
already does.

## Decision

**Finding Q — a mapping's provenance is scoped to the marker's own recorded
branch, not the scanning head.** That scoping governs both reconstruction
after a source-branch deletion and force-rebuild invalidation. Reconstruction
lists dest's branches directly (`git::remote_branch_names`) rather than only
the branches source still has, so a mirror-only branch's dest ref keeps
contributing its `SourceToDest` mappings after its local source branch is
deleted — the same routine post-merge cleanup decisions/0018 Case 2
describes, here deleting the local source branch while the dest ref
survives. Verification of a marker inherited this way is against the
marker's own recorded branch (`marker::verify_self`), never against the
branch whose history is currently being scanned: once the owning branch is
gone from source, a descendant's own first-parent scan is the only
remaining path to that marker, and the HMAC already authenticates the
branch recorded inside it regardless of which head is doing the scanning.
Without this, a deleted mirror-only branch's dest content would stop being
reachable through the mapping index at all, and a sibling forked from it
would fall back to a coarser graft and duplicate that content.
Tests: `mapping_index_self_verifies_a_source_marker_against_its_own_recorded_branch`
and `invalidate_replaced_chain_drops_only_the_named_branchs_unreachable_mappings`
(`src/commands/sync/mapping_index.rs:1576` and `:1373`, the latter proving
force-rebuild invalidation is scoped to the rebuilt branch); end-to-end in
`run_keeps_a_surviving_childs_dest_native_content_after_its_parent_branch_is_deleted`
and `run_anchors_a_sibling_on_a_deleted_mirror_only_branchs_own_dest_ref`
(`src/commands/sync/tests/anchor.rs`).

**Finding R — a mapping whose canonical dest object is absent locally is a
per-branch refusal.** Existence of a mapping's canonical dest commit is
checked with `Repository::find_commit` before any `graph_descendant_of`
comparison runs against it, both when resolving an anchor and when
canonicalizing among several mappings for the same exact source commit. A
missing canonical destination halts that one branch (`MappingLookup::
Contradictory`); the walk does not continue past it to an older mapping.
Walking past it would be unsafe: an older mapping can omit dest-native
content a `DestToSource` marker on the missing commit represents, and
replay would then silently loop-prevent something that was never actually
carried forward. This is already partially described in Addendum 1's last
paragraph (missing canonical destinations halt rather than being walked
past).
Tests: `nearest_mapping_halts_on_a_source_commit_whose_only_mapped_dest_is_missing_locally`
and `nearest_mapping_refuses_rather_than_erroring_when_one_of_several_mappings_is_missing_locally`
(`src/commands/sync/mapping_index.rs:1647` and `:1676`).

**Finding S — self-exclusion during canonicalization for `exclude_branch`'s
own rebuild.** When `resolve_for_anchor` is resolving an anchor for
`exclude_branch`'s own rewrite, a destination whose only provenance is
`exclude_branch` itself is dropped from canonicalization whenever any other
provenance exists for the same exact source commit — that branch's own
chain is exactly what the rewrite is discarding, so it is never a valid
anchor for its own rebuild. If every mapped destination for that source
commit is `exclude_branch`'s own, there is nothing to prefer instead: it is
untouched ancestor content (for example an earlier, still-valid part of the
branch's own chain that an amend never reached), not part of the chain being
discarded, and it is kept as a normal, usable mapping. Separately,
`record_built_mapping` records a `(source, dest)` pair `build_dest_commit`
just authored for `branch` this run without re-reading the commit or
re-verifying its marker: the caller already knows the exact tuple by
construction, having just built it.

CODE-004 records this rule's trade-off, which has two shapes depending on
which side is the ancestor. When the branch's own projection `D_own` is an
ancestor of the sibling's `D_sib`, self-exclusion drops `D_own` and the
anchor jumps forward to `D_sib` instead, importing sibling-only ancestry
into the rebuild — the case CODE-004's recommended Option A addresses, by
retaining an own-only destination when it is the ancestor of every other.
When `D_sib` is instead the ancestor of `D_own`, self-exclusion still drops
the (descendant) `D_own` and anchors on `D_sib`, so a force-rebuild may
rewind dest further than the amend that triggered it required; Option A
leaves this second case unchanged, since rule 3 would already select
`D_sib` once `D_own` is excluded. Both are safe under decisions/0038 (a
mirror-only rebuild is a force-with-lease) and are accepted for now;
CODE-004's plan (`docs/plans/2026-09-02/CODE-004-exclude-branch-own-anchor.md`)
holds the alternative of keeping the own projection in the first case,
where it is the common ancestor.
Tests: `resolve_for_anchor_excludes_the_current_branchs_own_sole_projection_of_a_shared_ancestor`
and `resolve_for_anchor_keeps_the_current_branchs_own_mapping_when_no_alternative_exists`
(`src/commands/sync/mapping_index.rs:1705` and `:1784`).

## Why

These three rules were already load-bearing implementation behaviour with
regression tests pinning them; only their record in `design/` was missing.
Naming them here, rather than leaving them as bare code citations to a
decision file that didn't define them, keeps `design/decisions/` the actual
source of truth AGENTS.md requires, and removes the collision with this
decision's own Addendum 1/2 finding letters.

## Consequences

* No behaviour change. Every citing code comment is updated from the old
  three-letter shorthand (Context, above) to `decisions/0046 Addendum 3,
  Finding Q`/`Finding R`/`Finding S`.
* `design/decisions/index.md`'s entry for this decision notes that Addendum 3
  records Findings Q–S.
