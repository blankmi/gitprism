---
type: Decision
title: A discovered branch's dest anchor prefers the nearest already-mirrored branch over the Setup/DestToSource baseline
description: scan_for_dest_marker's Setup/DestToSource-trailer scan stays the fallback, but a branch forked from another already-mirrored branch (round-tripped or mirror-only) now anchors its dest chain on that branch's own mirror at their real merge-base, via git merge_base plus graph_descendant_of specificity comparison, instead of always falling back to the nearest round-tripped marker. Ambiguous ties (two candidates whose merge-bases are neither ancestor nor descendant of each other) hard-fail, naming both.
tags: [architecture, branches, merge-base, filtering]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-27T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-27T00:00:00Z }
---

# Context

Real deployment shape: a round-tripped `main`, a mirror-only `feature` branched
from `main`, and a mirror-only `task` branched from `feature` — several task
branches PR'd against `feature` in Azure DevOps, not against `main`.

Every commit gitprism writes to dest is single-parent
([`build_dest_commit`](../decisions/0016-both-directions-merge-via-real-git-merge-tree.md),
`repo.commit(..., &[&parent_commit])`), and `pending_commits` walks
first-parent-only ([decisions/0035](0035-pending-history-is-first-parent.md)).
So when `task` is discovered ([decisions/0017](0017-source-to-dest-mirrors-every-branch.md)),
what dest-space point its new chain gets built onto is decided entirely by
`scan_for_dest_marker` (`src/commands/sync.rs`, feeding
`newest_dest_marker_opt_for_branch`), which walks `task`'s own first-parent
source history for the nearest commit carrying a `MarkerDirection::Setup` or
`MarkerDirection::DestToSource` trailer. Both trailers exist only on
**round-tripped** branches (the original `setup` graft, or a dest→source
import) — a mirror-only branch's own commits never carry one, since gitprism
never edits a commit it didn't create ([decisions/0003](0003-mapping-state-in-commit-trailers.md)).

`feature`'s own commits therefore carry no marker `task`'s scan can find.
The scan walks past all of `feature`'s history and resolves to whatever
`main` last round-tripped — a point at or before `feature`'s own fork from
`main`, not `task`'s real fork point from `feature`. `task`'s dest mirror
is then built as its own flattened chain rooted there, re-mirroring all of
`feature`'s history as brand-new commit objects with no ancestry in common
with `feature`'s own, separately-mirrored dest branch. Azure's PR diff for
`task → feature` has no real merge-base beyond that old point, so everything
`feature` already contributed shows up again as new/"already merged" commits
in the PR.

This same function feeds two call sites: a brand-new branch's first mirror
(`!dest_ref_exists`) and a detected mirror-only rewrite's rebuild
([decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md)).
Both inherit the fix below unchanged, since both already delegate to
`newest_dest_marker_opt_for_branch`.

**Prior art checked, none found.** None of `design/references/`'s five prior
tools (josh, Copybara, git-subtree, git-filter-repo, jujutsu) discover ad-hoc
branches the way [decisions/0017](0017-source-to-dest-mirrors-every-branch.md)
does, so none has an equivalent "which mirrored branch did this one fork
from" question to answer. The nearest existing precedent inside this
codebase is `already_merged_into_a_landing_branch` (`sync.rs:984`), which
already uses `repo.merge_base` plus `git::merge_tree` to compare a branch
against a *fixed* candidate set (`config.branches`) for a different question
(is this branch fully absorbed, safe to stop mirroring). This decision reuses
the same primitive — `merge_base` — for a different question (which existing
mirror is the closest parent) against a *dynamic* candidate set (every
branch with an existing dest ref, not just `config.branches`).

# Decision

Before falling back to `scan_for_dest_marker`'s existing trailer walk
("the baseline"), a discovered branch's anchor computation also searches for
a more specific anchor among sibling branches:

1. Compute the baseline unchanged: `scan_for_dest_marker(source_tip)` →
   `(boundary_base, dest_tip_base)`, or `None` (decisions/0024's existing
   warn-and-continue path, untouched).
2. Enumerate every other branch discovered on source this run
   ([decisions/0017](0017-source-to-dest-mirrors-every-branch.md)'s existing
   discovery, not a new listing mechanism) whose dest ref already exists.
   Round-tripped and mirror-only branches are both eligible candidates — no
   special-casing; the algorithm only ever resolves to the real historical
   merge-base with a candidate, never to that candidate's current tip, so
   anchoring on a round-tripped branch here is exactly as safe as anchoring
   on a mirror-only one. Dest-ref existence for every branch is resolved
   through one cache, populated once per `run()` invocation and updated
   in-memory after each successful push, not a fresh `git::remote_ref_exists`
   subprocess per candidate per branch — this search runs once per
   newly-discovered-or-rewritten branch, so querying per candidate would be
   O(branch count²) real network round trips per run instead of O(branch
   count).
3. For each candidate `C`, `cbase = repo.merge_base(new_branch_tip, C_tip)`;
   skip `C` on no shared history (same guard `already_merged_into_a_landing_branch`
   already uses). Discard any `cbase` that is not a descendant of (or equal
   to) `boundary_base` — a candidate less specific than what the baseline
   already found is never an improvement, and this bounds the search to
   real refinements only.
4. A survivor whose `cbase` merely *ties* `boundary_base` exactly offers no
   refinement at all — only survivors strictly beyond `boundary_base` are
   real candidates to disambiguate among. If none exist, use the baseline
   unchanged, same as "no survivors." Otherwise, among the survivors that do
   go beyond `boundary_base`, find the unique most-specific one: `C`'s
   `cbase` must be a descendant of (or equal to) every other survivor's
   `cbase`, via `graph_descendant_of` (the same primitive
   [decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md)'s
   condition 4 already uses). Exactly one maximal survivor → use it. **Two or
   more incomparable maximal survivors → hard-fail**, naming both candidate
   branches and their `cbase` oids; no guessing, matching this project's
   existing no-merge-base and no-lease precedent
   ([decisions/0007](0007-conflict-policy-hard-stop.md),
   [decisions/0023](0023-setup-reconciles-pre-existing-branches-via-merge-base.md)).
   Without excluding baseline-ties first, two unrelated siblings that each
   merely share `boundary_base` itself (offering nothing beyond what the
   baseline already found) would read as a spurious ambiguity. Two or more
   survivors sharing the exact same `cbase` (not just tying `boundary_base`,
   but tying *each other*) are likewise not ambiguous — equal is the
   opposite of incomparable. Collapse them to one representative (the
   lexicographically smallest branch name, for determinism) before the
   domination comparison; only survivors whose `cbase`s are genuinely
   incomparable after that collapse are ambiguous.
5. For the winning candidate `C`, locate the dest-space anchor: walk `C`'s
   fetched dest tip's history, first-parent
   ([decisions/0019](0019-marker-scans-are-first-parent-only.md)'s idiom,
   reused rather than re-derived — a round-tripped candidate's dest history
   can contain real human merges gitprism didn't build), for the newest
   commit whose `Gitprism-Source-Commit` trailer names a commit that is
   `cbase` itself or an ancestor of it (`marker::verify` with
   `MarkerDirection::SourceToDest`, the same primitive `pending_commits`'
   loop-prevention check already calls). That commit's source oid becomes
   the new boundary; its own oid becomes the new chain's starting parent —
   in place of `boundary_base`/`dest_tip_base`, with `pending_commits` and
   `build_pending_dest_tip` unchanged downstream of that substitution.

**Not solved here — accepted, same as decisions/0038/0039's own accepted
hazards:** if `C` (e.g. `feature`) hasn't been mirrored to dest yet in *this*
run when `task` is discovered, `task` falls back to the baseline this run —
and this search only ever runs again for `task` on a brand-new branch's
first mirror or on a positively *detected rewrite* of `task`'s own source
history (decisions/0039). An ordinary later resync of `task`, with `task`
itself unchanged, takes `dest_resume_point_for_branch`'s path instead
(`task`'s existing dest chain is still self-consistent, just anchored on the
coarser baseline), which never re-examines siblings. So **`task` does not
self-correct automatically once `feature` is mirrored** — deliberately not
solved here, since the only way to make it self-correct would be a new
automatic trigger ("a sibling's topology improved, force-rebuild onto it")
beyond decisions/0039's four-condition rewrite detection, and that trigger
would force-rewrite an already-open PR's history with no signal from
`task`'s own developer that anything about `task` should change — exactly
the "novel automation" this project's operator-intervention default
(`AGENTS.md`) reserves for an explicit decision, not a default. The operator
workaround is the same one decisions/0039's rewrite detection is already
built to recognize: `git commit --amend --no-edit` or `git rebase
--force-rebase` on `task` (or any other real change to `task`'s own source
history) is a positively detected rewrite, and that rebuild picks up the
now-available, more specific sibling anchor through this same search. No
branch-processing-order guarantee is added, and none is implied by
"self-corrects."

# Why

* **Git already has the primitive; this only asks it a second question.**
  `merge_base` + `graph_descendant_of` already answer "how does this branch
  relate to that one" everywhere else in this codebase
  ([decisions/0006](0006-setup-uses-real-shared-history.md),
  [decisions/0023](0023-setup-reconciles-pre-existing-branches-via-merge-base.md),
  [decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md)).
  Nothing new is added to the toolbox, only a wider candidate set for a
  question already being asked.
* **Bounded to strict refinement, never regression.** Step 3's descendant
  filter means this can only produce an anchor at least as specific as what
  already works today; a bug in the new search degrades to the existing
  baseline, not to a worse or unsafe result.
* **Resolving to the historical merge-base, not a candidate's live tip,
  keeps this safe for round-tripped candidates too.** Anchoring `task` on
  `main`'s *current* tip would silently splice `main`'s later, independent
  history into `task`'s dest ancestry — content `task` never actually
  descended from on source. Anchoring on `main`'s dest commit for
  `merge_base(task, main)` avoids that entirely; the same property that
  makes mirror-only candidates safe makes round-tripped ones safe too, so
  neither needs separate-casing.
* **Hard-fail on ambiguity, not silent fallback**, per this project's
  standing default (`AGENTS.md`: "if resolution requires guessing intent...
  fail clearly and let the operator resolve it") and its direct precedent
  in [decisions/0023](0023-setup-reconciles-pre-existing-branches-via-merge-base.md)'s
  own no-merge-base hard-fail — decided explicitly in conversation rather
  than defaulted to the alternative (quietly falling back to the baseline),
  which would hide a real structural ambiguity instead of surfacing it.

# Consequences

* `scan_for_dest_marker` itself is unchanged; this is a new step that runs
  before its result is accepted, not a rewrite of it.
* Both call sites of `newest_dest_marker_opt_for_branch`
  (`!dest_ref_exists` and decisions/0039's rewrite-rebuild arm) get the
  improved anchor automatically, with no separate implementation.
* **New failure mode to test and document**: a discovered branch with two
  incomparable most-specific mirrored ancestors hard-fails the branch (not
  the whole run — matching decisions/0024's per-branch-warning precedent
  for other structural surprises), naming both candidates.
* **Ordering hazard, accepted, permanently absent a later rewrite of the
  misanchored branch itself** — see "Not solved here" above. A branch
  discovered before its own parent branch has a dest ref falls back to the
  coarser baseline, and an ordinary later resync does not retry the search;
  only a positively detected rewrite of the branch itself does.
* **Dest-ref existence is a per-run cache**, not a fresh subprocess per
  sibling candidate — required to keep this search's cost linear rather than
  quadratic in branch count (see Decision step 2). A cache entry for the
  branch currently being synced must be invalidated on decisions/0009's own
  `RejectedRefMoved` race-retry, or a stale "no dest ref yet" entry from
  before a concurrent writer's push would survive into the retry and defeat
  it — this is decisions/0009's existing race-recompute invariant, not new
  behavior; the cache must not regress it.
* Tests the implementation commit must add:
  * `task` branched from mirror-only `feature` branched from round-tripped
    `main`: `task`'s dest chain anchors on `feature`'s dest tip, not `main`'s;
    a PR-shaped diff (`task`'s commits only) is asserted via the resulting
    dest tree/history, not just final content;
  * the same topology processed in the "wrong" order (`task` mirrored before
    `feature` has a dest ref): falls back to the baseline, and a real second
    sync of `task` (unchanged, not rewritten) stays on the baseline too — the
    absence of self-correction is itself the behavior under test;
  * a rewrite of `task` itself, after the wrong-order case above, does pick
    up `feature` as the more specific anchor — the documented operator
    workaround actually works;
  * a genuinely ambiguous case (two mirror-only branches, neither an ancestor
    of the other in `task`'s history) hard-fails naming both;
  * two sibling candidates that share the exact same `cbase` are *not*
    reported as ambiguous;
  * a round-tripped candidate correctly used as the anchor when no more
    specific mirror-only candidate exists (baseline and the new search agree,
    confirming no regression on the common case);
  * decisions/0039's rewrite-rebuild path exercised with a sibling mirror-only
    branch available as the more specific anchor, confirming the shared call
    site benefits without a second implementation.
