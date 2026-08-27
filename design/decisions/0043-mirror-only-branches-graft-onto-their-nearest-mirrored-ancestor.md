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
   go beyond `boundary_base`, **group them by `cbase`** — two or more
   survivors sharing the exact same `cbase` (not just tying `boundary_base`,
   but tying *each other*) are not ambiguous by merge-base, since equal is
   the opposite of incomparable, but every branch name in such a group must
   be *kept*, not collapsed to one representative: which of them, if any,
   actually has a usable dest-side marker is a question step 5 below has to
   answer per candidate, not one this step can shortcut by picking a name
   arbitrarily (e.g. lexicographically) before step 5 ever runs — a
   representative chosen that way can easily be the one candidate whose own
   dest history happens to lack a qualifying marker, silently discarding a
   sibling that would have found one. Find the unique most-specific group:
   its `cbase` must be a descendant of (or equal to) every other group's
   `cbase`, via `graph_descendant_of` (the same primitive
   [decisions/0039](0039-mirror-only-source-rewrites-rebuild-the-projection.md)'s
   condition 4 already uses). Exactly one maximal group → its branch names
   all proceed to step 5 together. **Two or more incomparable maximal
   groups → hard-fail**, naming every candidate in every such group and its
   `cbase` oid; no guessing, matching this project's existing no-merge-base
   and no-lease precedent ([decisions/0007](0007-conflict-policy-hard-stop.md),
   [decisions/0023](0023-setup-reconciles-pre-existing-branches-via-merge-base.md)).
   Without excluding baseline-ties first, two unrelated siblings that each
   merely share `boundary_base` itself (offering nothing beyond what the
   baseline already found) would read as a spurious ambiguity.
5. For every branch name in the winning group (not just one), fetch its dest
   tip through the same per-run cache as step 2's dest-ref-existence check
   (a fetch on a cache miss, remembered for every later lookup this run —
   trying every group member must not mean a fresh `git fetch` per member
   per branch searched, or this step's own fix for the equal-`cbase` bug
   just reintroduces the quadratic network cost this decision already
   fixed once, via a more expensive subprocess than before) and locate the
   dest-space anchor: walk its dest tip's history, first-parent
   ([decisions/0019](0019-marker-scans-are-first-parent-only.md)'s idiom,
   reused rather than re-derived — a round-tripped candidate's dest history
   can contain real human merges gitprism didn't build), for the newest
   commit whose `Gitprism-Source-Commit` trailer names a commit that is
   `cbase` itself or an ancestor of it (`marker::verify` with
   `MarkerDirection::SourceToDest`, the same primitive `pending_commits`'
   loop-prevention check already calls). A candidate whose own dest history
   has nothing qualifying (e.g. it already anchored onto *another* member of
   the same group during its own earlier sync, and so carries no
   independent marker of its own for `cbase` — the ordinary, expected shape
   once decisions/0043 has been running for a while) simply contributes
   nothing and is skipped, not treated as a failure. Among the candidates
   that *did* find something: if none did, degrade to the baseline
   (step 3's own safe-degrade). If every one that found something agrees on
   the identical `(source oid, dest oid)` pair, use it — this is the
   overwhelmingly common case once decisions/0043's own recursive anchoring
   is in effect, since a later sibling in the same group ordinarily just
   built onto an earlier one rather than re-projecting `cbase` itself. **If
   two or more disagree — genuinely different dest-space anchors for the
   same source-side `cbase`, which can only happen when they were populated
   independently of each other (e.g. two clones that never saw each other's
   dest ref) — hard-fail**, naming every disagreeing candidate and its own
   resolved dest oid; the same no-guessing default as step 4's hard-fail,
   just discovered one step later. The resolved `(source oid, dest oid)`
   pair — whichever way it was reached — becomes the new boundary and dest
   chain's starting parent, in place of `boundary_base`/`dest_tip_base`,
   with `pending_commits` and `build_pending_dest_tip` unchanged downstream
   of that substitution.

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
* **Two new failure modes to test and document**, both a per-branch halt
  (not the whole run — matching decisions/0024's per-branch-warning
  precedent for other structural surprises), naming every candidate
  involved: a discovered branch with two incomparable most-specific
  mirrored ancestors (step 4), and two equally specific candidates whose
  own dest histories resolve `cbase` differently (step 5) — genuinely
  distinct situations discovered at different points in the search, so
  reported with distinct messages naming different things (merge-base oids
  for the first, resolved dest oids for the second) rather than conflated
  into one.
* **A representative chosen before step 5 runs is unsound** — caught in
  review of the first implementation of this decision: collapsing an
  equal-`cbase` group to one branch name (e.g. the lexicographically
  smallest) before checking whether that specific candidate's own dest
  history has anything useful can silently discard a sibling that *does*
  have a usable marker, falling back to the coarser baseline even though a
  correct, more specific anchor exists. This is the ordinary shape once
  this decision has been in effect for a while: a later sibling in an
  equal-`cbase` group typically already anchored onto an earlier one during
  its own sync (this same mechanism, recursively), so it carries no
  independent marker of its own — trying every group member and skipping
  the ones that find nothing (step 5, revised) is required, not optional.
* **Ordering hazard, accepted, permanently absent a later rewrite of the
  misanchored branch itself** — see "Not solved here" above. A branch
  discovered before its own parent branch has a dest ref falls back to the
  coarser baseline, and an ordinary later resync does not retry the search;
  only a positively detected rewrite of the branch itself does.
* **Every real subprocess this search would otherwise repeat per candidate
  per branch is a per-run cache**, not just dest-ref existence: a fetched
  candidate's dest tip is cached too (see Decision steps 2 and 5), required
  to keep this search's cost linear rather than quadratic in branch count.
  Caught in review after the equal-`cbase` fix (step 5, trying every group
  member) reintroduced exactly this cost via `git fetch` instead of
  `ls-remote` — a second quadratic regression on top of the first, from a
  fix aimed at correctness rather than cost, which is precisely why cost
  must be re-checked every time a correctness fix touches this search's own
  candidate loop. A cache entry for the branch currently being synced must
  be invalidated on decisions/0009's own `RejectedRefMoved` race-retry, or a
  stale "no dest ref yet" entry from before a concurrent writer's push would
  survive into the retry and defeat it — this is decisions/0009's existing
  race-recompute invariant, not new behavior; the cache must not regress it.
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
  * two sibling candidates that share the exact same `cbase`, where one
    already anchored onto the other during its own earlier sync (so only
    one has a usable marker), resolve onto the one that does — not the
    lexicographically first, and not a silent fallback to the baseline;
  * two sibling candidates that share the exact same `cbase` but were
    populated independently and resolve to genuinely different dest oids
    hard-fail, naming both candidates and their own resolved anchors;
  * a round-tripped candidate correctly used as the anchor when no more
    specific mirror-only candidate exists (baseline and the new search agree,
    confirming no regression on the common case);
  * decisions/0039's rewrite-rebuild path exercised with a sibling mirror-only
    branch available as the more specific anchor, confirming the shared call
    site benefits without a second implementation;
  * a fetched candidate's dest tip is cached: a second lookup for the same
    branch, against a deliberately unreachable remote, must still succeed
    with the identical oid — proof positive it never fetched again, not
    just absence of a slowdown.

# Addendum (2026-08-27): a branch-scoped `DestToSource` marker is recognized by every branch it's an ancestor of

## Context

A repository review (finding F-04) reproduced a duplicate-commit bug for
the most common topology this decision exists to fix: `task` forked from
round-tripped `main` *after* a dest-native commit *X* (committed directly
to dest, outside gitprism) was reflected into source by dest→source as
marker commit *M* on `main` (`Gitprism-Dest-Commit: X`,
`MarkerDirection::DestToSource`, `Gitprism-Branch: main`). `task` inherits
*M* as an ordinary ancestor — it's just a commit on `main`'s history that
`task` branched from.

Two independent places reject *M* for `task`, for the same underlying
reason: `marker::verify` (`src/marker.rs`, ~line 320) requires an exact
match between the commit's own recorded `Gitprism-Branch` and the branch
name the caller asks about — deliberately, for every direction except
`Setup`, which every branch inherits regardless of name
(decisions/0017). *M* is scoped to `"main"`, not `"task"`.

* `build_pending_dest_tip`'s loop prevention (decisions/0003) calls
  `marker::verify(&source_commit, branch, [Setup, DestToSource], ...)`
  with `branch = "task"` — fails, so *M* is not recognized as already on
  dest and gets replayed as an ordinary new commit, filtering to a
  duplicate of *X* on dest's `task`.
* This decision's own step 5 (`newest_source_marker_at_or_before`) only
  ever looks for `MarkerDirection::SourceToDest` trailers, walking a
  sibling candidate's *dest*-side history — it can never find *M* at all,
  since *M* lives on *source*, not dest. Even when `main` is found as
  `task`'s sibling candidate and `cbase` lands exactly on *M*, step 5
  still resolves to whatever *earlier* `SourceToDest` marker dest's own
  history happens to carry, silently choosing a less specific anchor than
  the one *M* itself already proves.

Both gaps have the same root cause and the same fix: *M*'s own MAC already
proves, independent of which branch asks, that dest genuinely has *X* —
that fact doesn't become false because a *different* branch happens to be
doing the asking.

## Decision

When a `DestToSource` marker commit is an ancestor of the branch currently
being processed, and the MAC verifies for the marker's *own* recorded
branch (never for the caller's branch — `marker::verify` itself is
unchanged, still exact-match, still exactly as strict as decisions/0025
requires), that marker:

1. **Counts for loop prevention, for any branch it is an ancestor of.**
   `build_pending_dest_tip` (and `find_control_file_policy_mismatch`'s own
   parallel loop-prevention skip, decisions/0037, which must stay
   consistent with what will and won't actually be replayed) now go
   through one shared helper, `loop_prevented`: first the existing
   branch-scoped check (`Setup`/`DestToSource` matched against the caller's
   `branch`, unchanged); if that fails, parse the commit's message
   (`marker::parse`, already `pub(crate)`) to read its own recorded branch,
   then re-verify with *that* branch name and `DestToSource` only (`Setup`
   already inherits unconditionally, so it needs no widening here).
   `marker::verify`'s internal MAC payload is always computed over the
   marker's own recorded branch regardless of which branch name is passed
   in for the filter check — passing the marker's own branch back to itself
   is exactly "does this marker's own MAC verify," nothing weaker.
2. **Is usable by step 5 as an anchor**, resolving directly to the dest
   commit it names. A new function, `newest_dest_to_source_marker_at_or_before`,
   walks a candidate's own *source*-side history first-parent from `cbase`
   backward (the same shape `scan_for_dest_marker` already walks for the
   baseline scan, but deliberately `DestToSource`-only — `Setup` excluded,
   see Why) for the newest marker scoped to that candidate's own name.
   Tried per candidate alongside the existing dest-side scan
   (`newest_source_marker_at_or_before`); whichever of the two is strictly
   more specific (its own source oid equal to `cbase`, or a descendant of
   the other's) wins; if only one found anything, it wins by default; if
   neither did, the candidate contributes nothing, unchanged from today.

## Why

* **`marker::verify` itself is not weakened.** The task that produced this
  addendum was explicit that the MAC verification scope must stay exactly
  as strict, checked against the marker's own recorded branch — never
  against the branch asking. Both changes above only ever call
  `marker::verify` with a branch name read *from the marker itself*
  (`marker::parse`), so a forged or copied trailer still can't pass: the
  MAC is computed over the marker's own recorded fields regardless of which
  name is handed to `verify`, so misrepresenting the branch buys an
  attacker nothing new.
* **Why `Setup` is excluded from the new source-side scan.** `Setup` is
  already unconditionally inherited by every branch
  (`marker::verify`'s own hardcoded exception, decisions/0017) — every
  candidate branch's own history trivially reaches the one shared graft
  commit, regardless of name. Accepting it in
  `newest_dest_to_source_marker_at_or_before` would rediscover the same
  coarse point `boundary_base` already reflects for *every* candidate, not
  a real per-candidate refinement — caught in review of the first
  implementation of this addendum: two equal-`cbase` sibling candidates
  with no `DestToSource` marker of their own started reading as
  *disagreeing* (`AmbiguousResolution`) instead of both correctly
  contributing nothing, because each one's "finding" was really just the
  shared graft in disguise. Loop prevention has no equivalent hazard:
  `marker::verify` already inherits `Setup` unconditionally there, so
  `loop_prevented`'s own first check already covers it before the
  `DestToSource`-only widening is ever reached.
* **Why loop prevention needed its own fix, not just step 5's.** Step 5
  only runs for a brand-new branch's first mirror or a positively detected
  rewrite (this decision's own scope) — an *ordinary* resync of an
  already-anchored branch never re-runs it (see "Not solved here" above).
  If step 5 ever resolves onto a coarser anchor than *M* — the documented
  wrong-order ordering hazard this decision already accepts, or simply a
  candidate branch not yet processed this run — *M* still ends up inside
  that branch's own `pending_commits` range regardless of how the anchor
  was chosen, and only loop prevention itself can then recognize it.
  Reasoned, not assumed: reproduced independently by forcing the wrong-order
  fallback via `RunCache::dest_ref_exists` and confirming loop prevention
  alone (step 5's fix reverted) still closes it.
* **Git already proves this; nothing new is trusted.** Reasoned the same
  way this decision's own body reasons about `merge_base`: the MAC already
  proves *M* is authentic and its `DestToSource` direction already means
  "the named dest commit is already on dest" (decisions/0003) — this
  addendum only widens *which branch's replay loop* is allowed to act on a
  fact that was already true regardless of the asking branch, it does not
  ask git or the MAC to prove anything new.

## Consequences

* One new shared helper, `loop_prevented` (`src/commands/sync.rs`), used by
  both `build_pending_dest_tip` and `find_control_file_policy_mismatch` —
  one definition of "already on dest," not two that could drift.
* One new function, `newest_dest_to_source_marker_at_or_before`, alongside
  the existing `newest_source_marker_at_or_before` it's tried against —
  `scan_for_dest_marker` itself stays untouched (per this decision's own
  original "unchanged" guarantee for the baseline scan), since a
  `Setup`-accepting scan is the wrong tool for this specific,
  per-candidate refinement.
* No new fetches: `newest_dest_to_source_marker_at_or_before` walks only
  already-local source-side history, seeded at `cbase` (already resolved
  by this point in the search) — it runs alongside, not instead of, the
  existing dest-side fetch, so decisions/0043's own O(branch count) cost
  claim is unaffected.
* **Tests added:**
  * `run_task_forked_from_main_after_a_dest_native_import_does_not_duplicate_it`
    (`src/commands/sync.rs`) — the full reproduction, via `run()` end to
    end: dest-native *X*, imported as *M* on `main`, `task` forked
    afterward. Confirmed failing (two commits beyond dest `main`'s tip
    instead of one) against the pre-fix code, and passing after.
  * `build_pending_dest_tip_loop_prevents_a_dest_to_source_marker_scoped_to_another_branch`
    (`src/commands/sync.rs`) — isolates loop prevention from the step-5
    fix by forcing the wrong-order baseline fallback (`main` hidden from
    the sibling search via `RunCache::dest_ref_exists`, so *M* is
    necessarily inside `task`'s own `pending_commits` range regardless of
    anchor precision). Confirmed failing with only the step-5 fix reverted
    (three replayed commits instead of two), and passing with both fixes
    in place.
