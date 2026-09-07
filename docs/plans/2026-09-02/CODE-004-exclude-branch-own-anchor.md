# Plan CODE-004 — keep a rewritten branch's own comparable anchor

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, CODE-004 |
| Severity / priority | LOW / P2 |
| Effort | Small (step 1); Medium (step 2) |
| Decision required | Step 1: no (recorded by DOC-001). Step 2: yes — changes anchor selection, so an addendum to 0046 before code |
| Depends on | DOC-001 completed; optional step 2 follows CODE-010 authorization decision |
| Status | Step 1 completed 2026-09-04 through DOC-001; current behavior accepted in decision 0046 Finding S. Step 2 remains an optional owner decision |

## Problem

`resolve_for_anchor` (`src/commands/sync/mapping_index.rs:226-235`) drops every
destination whose only provenance is `exclude_branch` as soon as any other
provenance exists for the same source commit. When the nearest mapped source
commit `S` is untouched by the rewrite, its own dest commit `D_own` is still
valid content. Two cases:

* `D_own` is an ancestor of the sibling's `D_sib` (the sibling was cut from
  this branch). Rule 3 of 0046 would pick the common ancestor `D_own`; F-C picks
  `D_sib` and imports sibling-only ancestry into the rebuild.
* `D_sib` is an ancestor of `D_own` (this branch was cut from the sibling).
  F-C picks `D_sib` and re-creates every dest commit between them with new
  object ids, although their content is unchanged.

Both are safe under decision 0038 (mirror-only rebuild is a force-with-lease).
The behavior is now recorded in decision 0046 Addendum 3, Finding S.
This is accepted behavior, not an outstanding correctness defect. Decision
0050 (CODE-010) keeps the rebuild authorized and adds a source-remote tip
check in front of it; land that before optimizing the anchor of an
authorized rebuild.

## Steps

### Step 1 — record (do now)

Covered by DOC-001 step 1, Finding S. Nothing else.

### Step 2 — keep the own projection when it is the common ancestor (decide first)

**Files.** `design/decisions/0046-...md` (addendum), `mapping_index.rs:226-235`,
tests near `:1705` and `:1784`.

Retaining `D_own` in `by_dest` only changes the outcome in the first case. The
selection at `mapping_index.rs:286-307` is 0046 rule 3, "the destination that
is an ancestor of every other"; in the second case that is still `D_sib`, so
keeping `D_own` in the candidate set does nothing. Two options, and the plan
recommends option A:

**Option A (recommended) — first case only.** Retain an own-only destination
when it is an ancestor of every other destination for the same source commit;
drop it otherwise (incomparable, or descendant of a sibling's). Rule 3 then
selects `D_own` on its own. No new selection rule; the addendum records the
narrowed F-C.

**Option B — both cases.** Add a selection rule ahead of rule 3: if an own-only
destination is comparable with every other destination, select it regardless
of direction. This overrides rule 3 for the descendant case and the addendum
must say why an own-chain descendant is preferred over the common ancestor.
Not recommended: it trades a documented, uniform rule for fewer rewritten dest
commits.

**Test first.** Fixtures for both ancestry orders and for the incomparable
case. Under option A assert `D_own` in case 1, `D_sib` in case 2 (unchanged),
`D_own` dropped when incomparable. The legacy wrong-order test at `:1705` must
keep passing; if it cannot, the addendum records why and step 2 stops.

**Why decide first.** It changes which dest history a force-rebuild lands on.
The gain is fewer rewritten dest commits, not correctness. Under AGENTS.md's
rule, that is the project owner's call.

## Revision history

* 2026-09-02, after plan review: the first draft claimed `D_own` would be
  chosen in both ancestry orders by retaining it; rule 3 makes that true only
  when `D_own` is the ancestor. Rewritten as options A and B.

## Verification

Step 2: the three fixtures plus the full `mapping_index` and rewrite-matrix tests
(0046 Addendum 1, "Preserve established safety behavior").
