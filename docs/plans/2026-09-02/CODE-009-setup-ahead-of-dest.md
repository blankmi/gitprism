# Plan CODE-009 — setup on a branch strictly ahead of dest

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, informational CODE-009 |
| Severity / priority | INFO / P3 |
| Effort | Small |
| Decision required | Yes if step 2 is taken (changes decision 0023's output) |
| Depends on | — |
| Status | Proposed; recommendation is step 1 only |

## Problem

When a pre-existing source branch already descends from dest's tip, `setup`
(decision 0023 path, `src/commands/setup.rs:242-…`) still writes a two-parent
graft commit whose second parent is an ancestor of the first. Git itself would
say "already up to date". The commit is correct (it carries the `Setup` marker
naming dest's tip, which every later scan relies on) but renders as a
degenerate merge in graph views.

## Steps

### Step 1 — record (recommended)

**Files.** `design/decisions/0023-setup-reconciles-pre-existing-branches-via-merge-base.md`
(addendum).

**Change.** State the case, that the marker commit is required regardless of
ancestry, that the second parent is redundant but harmless, and that the shape
is kept for one reason: every consumer (`graft_point`, `dest_tip_accounted_for`
case 2, resume scans) treats the graft uniformly, and a single-parent variant
would be a second shape to test everywhere.

### Step 2 — single-parent marker commit (only if the owner prefers it)

**Test first.** Setup on a branch two commits ahead of dest produces a graft
with one parent, the branch's tip, tree equal to the tip's tree, and a verified
`Setup` marker naming dest's tip. Then a full sync round-trips normally, and
`graft_point` still finds the boundary.

**Change.** In the 0023 path, if `repo.graph_descendant_of(existing_oid,
dest_tip)?`, build the marker commit with `parents = [existing_oid]`. Audit
`graft_point` (it may use `merge_base`, which still works: the merge base of
the marker commit and dest's tip is dest's tip).

## Verification

Step 2: `cargo test setup` plus the full sync suite (the graft shape is an
input to almost every fixture).
